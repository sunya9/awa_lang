use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, Pattern, Stmt, TExpr, TExprKind, TStmt, TypeDef, TypeExpr};
use crate::error::AwaError;
use crate::match_check::{Sigs, check_matches};
use crate::token::Token;
use crate::types::{Scheme, Subst, Type, VarGen, apply, ftv, instantiate, render_type, unify};

// 型名 → 型パラメータの個数（arity）。型式の解決時に「Option(a) の引数個数が
// 宣言と合うか」を検査するのに使う。3b の網羅性チェックで variant 一覧へ拡張予定。
type Arities = HashMap<String, usize>;

// ===== 型推論（Algorithm W / let 多相つき Hindley-Milner）=====
// AST をたどって式の型を計算し、矛盾があれば unify が CannotUnify などを返す。
// let 束縛は generalize で多相化（型スキーム化）され、変数参照のたびに
// instantiate で fresh な型に具体化される。これにより Gleam/ML 同様、
// `let id = fn(x){x}` が ∀a. fn(a)->a になり Num にも Bool にも使える。
// awa は可変参照を持たない純粋な言語なので、value restriction なしに
// 全 let を自由に一般化しても健全。

// 型環境: 変数名 → 型スキーム。値の Env（名前→Value）の型版。
// 多相のため Type ではなく Scheme を持つ（単相な束縛は Scheme::mono）。
pub type TypeEnv = HashMap<String, Scheme>;

// 推論器の状態。3 つまとめて持つことで REPL が行をまたいで持ち越せる:
//   subst  : 確定した型変数の置換テーブル（推論の本体状態）
//   vargen : 型変数 ID の採番カウンタ（行をまたいで衝突させない）
//   env    : トップレベルの型環境（let で束縛した変数の型が溜まる）
#[derive(Default)]
pub struct Checker {
    subst: Subst,
    vargen: VarGen,
    env: TypeEnv,
    // 宣言済みの型名 → arity。構築子スキームは env に入るので別途は持たない。
    arities: Arities,
    // 網羅性チェック用の構築子シグネチャ（型名↔構築子↔arity）。REPL で行をまたいで持ち越す。
    sigs: Sigs,
}

impl Checker {
    pub fn new() -> Self {
        Checker::default()
    }

    // プログラム（トップレベルの文の並び）を型検査し、型付き AST（elaboration の結果）を返す。
    // eval_program はこの型付きプログラムを実行する。let の束縛・型宣言はトップレベルの
    // self に積まれ、次の行から参照できる。型宣言は値文を生まない（eval と整合）。
    pub fn check_program(&mut self, stmts: &[Stmt]) -> Result<Vec<TStmt>, AwaError> {
        let mut typed = Vec::new();
        for stmt in stmts {
            match stmt {
                Stmt::Let(name, annotation, expr) => {
                    let te = infer_let(
                        name,
                        annotation,
                        expr,
                        &mut self.env,
                        &mut self.subst,
                        &mut self.vargen,
                        &self.arities,
                    )?;
                    typed.push(TStmt::Let(name.clone(), te));
                }
                Stmt::Expr(expr) => {
                    let te = infer_expr(
                        expr,
                        &self.env,
                        &mut self.subst,
                        &mut self.vargen,
                        &self.arities,
                    )?;
                    typed.push(TStmt::Expr(te));
                }
                Stmt::Type(def) => {
                    register_type(
                        def,
                        &mut self.env,
                        &mut self.subst,
                        &mut self.vargen,
                        &mut self.arities,
                    )?;
                    // 網羅性チェック用のシグネチャも積む（トップレベル型は持ち越す）。
                    self.sigs.add(def);
                    typed.push(TStmt::Type(def.clone()));
                }
            }
        }
        // 型検査が通ったら、全 case 式の網羅性・到達不能を静的に検査する（untyped AST 上で）。
        check_matches(stmts, &self.sigs)?;
        // 最終的な置換を全ノードの型に適用して解決する（推論中は未確定変数が残るため）。
        for ts in &mut typed {
            resolve_tstmt(ts, &self.subst);
        }
        Ok(typed)
    }
}

// 型付き AST の全ノードの型に最終置換を適用して解決する（elaboration の仕上げ）。
fn resolve_texpr(te: &mut TExpr, subst: &Subst) {
    te.ty = apply(subst, &te.ty);
    match &mut te.kind {
        TExprKind::Num(_) | TExprKind::Bool(_) | TExprKind::Var(_) => {}
        TExprKind::Neg(x) => resolve_texpr(x, subst),
        TExprKind::BinOp(l, _, r) => {
            resolve_texpr(l, subst);
            resolve_texpr(r, subst);
        }
        TExprKind::If(c, t, e) => {
            resolve_texpr(c, subst);
            resolve_texpr(t, subst);
            resolve_texpr(e, subst);
        }
        TExprKind::Lambda(_, body) => resolve_texpr(body, subst),
        TExprKind::Call(callee, args) => {
            resolve_texpr(callee, subst);
            for a in args {
                resolve_texpr(a, subst);
            }
        }
        TExprKind::Block(stmts) => {
            for s in stmts {
                resolve_tstmt(s, subst);
            }
        }
        TExprKind::Case(subject, arms) => {
            resolve_texpr(subject, subst);
            for (_, body) in arms {
                resolve_texpr(body, subst);
            }
        }
    }
}

fn resolve_tstmt(ts: &mut TStmt, subst: &Subst) {
    match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => resolve_texpr(te, subst),
        TStmt::Type(_) => {}
    }
}

// 型宣言を処理する: 各型パラメータに fresh 型変数を割り当て、各構築子の関数型
// fn(field 型...) -> Con(型名, パラメータ) を組み立てて一般化し、構築子名で型 env に登録する。
// arity を先に登録するので、再帰 ADT（Cons(a, List(a)) が List を参照）も解決できる。
fn register_type(
    def: &TypeDef,
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &mut Arities,
) -> Result<(), AwaError> {
    // 型パラメータ名 → fresh 型変数。全 variant で共有する。
    let mut param_map = HashMap::new();
    let mut param_tys = Vec::with_capacity(def.params.len());
    for p in &def.params {
        let tv = vargen.fresh();
        param_map.insert(p.clone(), tv.clone());
        param_tys.push(tv);
    }
    // 再帰参照のため arity を先に登録する。
    arities.insert(def.name.clone(), def.params.len());
    let result = Type::Con(def.name.clone(), param_tys);
    for variant in &def.variants {
        let mut field_tys = Vec::with_capacity(variant.fields.len());
        for f in &variant.fields {
            field_tys.push(resolve_type_expr(f, &param_map, arities)?);
        }
        let ctor_ty = if field_tys.is_empty() {
            result.clone()
        } else {
            Type::Fun(field_tys, Box::new(result.clone()))
        };
        // 型パラメータは環境に縛られていないので generalize で全て量化される
        // （Some : ∀a. fn(a) -> Option(a)）。使うたび instantiate されて多相が効く。
        let scheme = generalize(env, subst, &ctor_ty);
        env.insert(variant.name.clone(), scheme);
    }
    Ok(())
}

// 型構築子名（と解決済み引数）を意味論的な Type へ。組込型 Num/Bool/Nil と
// 宣言済みの型名（arity 検査つき）を扱う。型定義・注釈の両方から使う共通部分。
fn resolve_con(name: &str, args: Vec<Type>, arities: &Arities) -> Result<Type, AwaError> {
    match name {
        "Num" if args.is_empty() => Ok(Type::Num),
        "Bool" if args.is_empty() => Ok(Type::Bool),
        "Nil" if args.is_empty() => Ok(Type::Unit),
        _ => match arities.get(name) {
            Some(&arity) if arity == args.len() => Ok(Type::Con(name.to_string(), args)),
            _ => Err(AwaError::UnknownType(name.to_string())),
        },
    }
}

// 型定義のフィールド型を解決する。型変数は宣言済みの param_map に限られる
// （未宣言の型変数はエラー）。
fn resolve_type_expr(
    texpr: &TypeExpr,
    param_map: &HashMap<String, Type>,
    arities: &Arities,
) -> Result<Type, AwaError> {
    match texpr {
        TypeExpr::Var(name) => param_map
            .get(name)
            .cloned()
            .ok_or_else(|| AwaError::UnknownType(name.clone())),
        TypeExpr::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_type_expr(a, param_map, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities)
        }
        TypeExpr::Fun(params, ret) => {
            let params = params
                .iter()
                .map(|p| resolve_type_expr(p, param_map, arities))
                .collect::<Result<_, _>>()?;
            let ret = resolve_type_expr(ret, param_map, arities)?;
            Ok(Type::Fun(params, Box::new(ret)))
        }
    }
}

// 型注釈（let/引数/戻り値）を解決する。型定義と違い、小文字の型変数は「その注釈
// （or シグネチャ）内で fresh な統一変数」になる。vars に同名で覚えておくことで、
// fn(x: a) -> a の 3 つの a が同じ変数を指す。推論結果と unify されて効く。
fn resolve_annotation(
    texpr: &TypeExpr,
    vars: &mut HashMap<String, Type>,
    vargen: &mut VarGen,
    arities: &Arities,
) -> Result<Type, AwaError> {
    match texpr {
        TypeExpr::Var(name) => {
            Ok(vars.entry(name.clone()).or_insert_with(|| vargen.fresh()).clone())
        }
        TypeExpr::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_annotation(a, vars, vargen, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities)
        }
        TypeExpr::Fun(params, ret) => {
            let params = params
                .iter()
                .map(|p| resolve_annotation(p, vars, vargen, arities))
                .collect::<Result<_, _>>()?;
            let ret = resolve_annotation(ret, vars, vargen, arities)?;
            Ok(Type::Fun(params, Box::new(ret)))
        }
    }
}

// 注釈を「rigid な skolem」付きで解決する。型変数を、ユーザーが書けない名前の
// nominal 構築子（#skN）に変える。別々の skolem は互いに unify できないので、
// 「注釈が推論型より一般的すぎる」を unify の失敗として検出できる（subsumption 検査）。
fn resolve_skolem(
    texpr: &TypeExpr,
    sk: &mut HashMap<String, Type>,
    vargen: &mut VarGen,
    arities: &Arities,
) -> Result<Type, AwaError> {
    match texpr {
        TypeExpr::Var(name) => Ok(sk
            .entry(name.clone())
            .or_insert_with(|| match vargen.fresh() {
                Type::Var(id) => Type::Con(format!("#sk{}", id), Vec::new()),
                _ => unreachable!(),
            })
            .clone()),
        TypeExpr::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_skolem(a, sk, vargen, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities)
        }
        TypeExpr::Fun(params, ret) => {
            let params = params
                .iter()
                .map(|p| resolve_skolem(p, sk, vargen, arities))
                .collect::<Result<_, _>>()?;
            let ret = resolve_skolem(ret, sk, vargen, arities)?;
            Ok(Type::Fun(params, Box::new(ret)))
        }
    }
}

// 型環境全体の自由型変数。各スキームの自由変数は ftv(ty) から量化変数を除いたもの
// （量化済みの変数は「環境に縛られている」とは見なさない）。
fn ftv_env(env: &TypeEnv, subst: &Subst) -> HashSet<u32> {
    let mut acc = HashSet::new();
    for scheme in env.values() {
        let mut vars = ftv(subst, &scheme.ty);
        for q in &scheme.vars {
            vars.remove(q);
        }
        acc.extend(vars);
    }
    acc
}

// 一般化（generalize）: 型の自由変数のうち、環境に自由出現しないものを量化して
// スキームにする。「周囲のスコープに縛られていない変数だけ」を多相にするのが肝。
// 例: トップレベルの id は fn(a)->a の a が環境に出てこないので ∀a. fn(a)->a になる。
//     一方 fn(y){ let f = fn(x){y}; f } の f は y 由来の変数が環境に縛られているので
//     その変数は量化されない（過剰一般化を防ぐ）。
fn generalize(env: &TypeEnv, subst: &Subst, ty: &Type) -> Scheme {
    let env_vars = ftv_env(env, subst);
    let ty = apply(subst, ty);
    let vars: Vec<u32> = ftv(subst, &ty)
        .into_iter()
        .filter(|id| !env_vars.contains(id))
        .collect();
    Scheme { vars, ty }
}

// let 束縛の型推論。再帰関数のために、本体を見る前に名前を fresh 型変数で
// 先に束縛しておく（letrec 的・単相 placeholder）。これにより
// `let fact = fn(n){ ... fact(n-1) ... }` の本体内で fact を参照でき、推論後に
// 「先に置いた型変数」と「本体から推論した型」を unify して辻褄を合わせる。
// eval 側の「名前を刻んで自己注入」と対になる仕組み。
// 最後に、自分自身の束縛を外した環境で generalize して多相スキームを env に積む。
fn infer_let(
    name: &str,
    annotation: &Option<TypeExpr>,
    expr: &Expr,
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &Arities,
) -> Result<TExpr, AwaError> {
    let placeholder = vargen.fresh();
    env.insert(name.to_string(), Scheme::mono(placeholder.clone()));
    let te = infer_expr(expr, env, subst, vargen, arities)?;
    unify(subst, &placeholder, &te.ty)?;
    // 自分自身の単相 placeholder を環境から外してから一般化する。
    // 外さないと placeholder が型変数を環境に縛り、何も量化されなくなる。
    env.remove(name);
    // 型注釈があれば突き合わせる。
    if let Some(ann) = annotation {
        // (1) 制約: 注釈型と推論型を unify。噛み合わない型（Num と Bool 等）はここで CannotUnify。
        let mut vars = HashMap::new();
        let ann_ty = resolve_annotation(ann, &mut vars, vargen, arities)?;
        unify(subst, &placeholder, &ann_ty)?;
        // (2) subsumption: 注釈が推論された principal 型より一般的すぎないか。
        // principal を instantiate し、注釈を skolem 化して別 subst 上で unify する。
        // 失敗＝注釈が実際には持っていない一般性を主張している。
        let principal = generalize(env, subst, &placeholder);
        let inst = instantiate(&principal, vargen);
        let mut sk = HashMap::new();
        let ann_skolem = resolve_skolem(ann, &mut sk, vargen, arities)?;
        let mut scratch = subst.clone();
        if unify(&mut scratch, &inst, &ann_skolem).is_err() {
            return Err(AwaError::AnnotationTooGeneral {
                inferred: render_type(&principal.ty),
                annotation: render_type(&ann_ty),
            });
        }
    }
    let scheme = generalize(env, subst, &placeholder);
    env.insert(name.to_string(), scheme);
    Ok(te)
}

// 式を型推論し、型付きノード（TExpr）を組んで返す（elaboration）。各ノードに
// その時点の型を載せる（未確定変数は check_program 末尾の解決パスで確定する）。
// env は読むだけ（スコープ拡張はクローン）。subst/vargen は進行に伴い更新する。
fn infer_expr(
    expr: &Expr,
    env: &TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &Arities,
) -> Result<TExpr, AwaError> {
    let (kind, ty) = match expr {
        Expr::Num(n) => (TExprKind::Num(*n), Type::Num),
        Expr::Bool(b) => (TExprKind::Bool(*b), Type::Bool),
        // 変数は型環境を引き、スキームを instantiate して fresh な型に具体化する。
        // 多相な束縛（構築子 Some 等も含む）はここで使用箇所ごとに独立した型変数を得る。
        // 無ければ評価時と同じ UndefinedVariable。
        Expr::Var(name) => {
            let ty = env
                .get(name)
                .map(|scheme| instantiate(scheme, vargen))
                .ok_or_else(|| AwaError::UndefinedVariable(name.clone()))?;
            (TExprKind::Var(name.clone()), ty)
        }
        // 単項マイナスはオペランドが Num、結果も Num。
        Expr::Neg(operand) => {
            let t = infer_expr(operand, env, subst, vargen, arities)?;
            unify(subst, &t.ty, &Type::Num)?;
            (TExprKind::Neg(Box::new(t)), Type::Num)
        }
        Expr::BinOp(left, op, right) => {
            let lt = infer_expr(left, env, subst, vargen, arities)?;
            let rt = infer_expr(right, env, subst, vargen, arities)?;
            let ty = infer_binop(&lt.ty, op, &rt.ty, subst)?;
            (TExprKind::BinOp(Box::new(lt), op.clone(), Box::new(rt)), ty)
        }
        // if: 条件は Bool、両腕は同じ型でなければならない。結果はその腕の型。
        Expr::If(cond, then_branch, else_branch) => {
            let ct = infer_expr(cond, env, subst, vargen, arities)?;
            unify(subst, &ct.ty, &Type::Bool)?;
            let tt = infer_expr(then_branch, env, subst, vargen, arities)?;
            let et = infer_expr(else_branch, env, subst, vargen, arities)?;
            unify(subst, &tt.ty, &et.ty)?;
            let ty = apply(subst, &tt.ty);
            (
                TExprKind::If(Box::new(ct), Box::new(tt), Box::new(et)),
                ty,
            )
        }
        // 無名関数: 各引数に型を割り当て（注釈があればその型、無ければ fresh）、
        // その下で本体を推論する。戻り型注釈があれば本体型と unify する。
        // 注釈中の型変数はこのシグネチャ内で共有する（sig_vars を引数・戻り型で使い回す）。
        Expr::Lambda(params, ret_annotation, body) => {
            let mut body_env = env.clone();
            let mut sig_vars = HashMap::new();
            let mut param_types = Vec::with_capacity(params.len());
            let mut param_names = Vec::with_capacity(params.len());
            for (name, annotation) in params {
                let ty = match annotation {
                    Some(ann) => resolve_annotation(ann, &mut sig_vars, vargen, arities)?,
                    None => vargen.fresh(),
                };
                // ラムダ引数は単相（多相化しない）。HM の多相境界は let だけ。
                body_env.insert(name.clone(), Scheme::mono(ty.clone()));
                param_types.push(ty);
                param_names.push(name.clone());
            }
            let body_t = infer_expr(body, &body_env, subst, vargen, arities)?;
            if let Some(ann) = ret_annotation {
                let ann_ty = resolve_annotation(ann, &mut sig_vars, vargen, arities)?;
                unify(subst, &body_t.ty, &ann_ty)?;
            }
            let param_types = param_types.iter().map(|t| apply(subst, t)).collect();
            let ty = Type::Fun(param_types, Box::new(apply(subst, &body_t.ty)));
            (TExprKind::Lambda(param_names, Box::new(body_t)), ty)
        }
        // 呼び出し: callee を fn(実引数型...) -> 新しい戻り型 と unify する。
        // unify が「callee が関数か」「引数の個数と型が合うか」を一括で検査する。
        // 戻り型は fresh 型変数で置き、unify 後に確定したものを返す。
        Expr::Call(callee, args) => {
            let callee_t = infer_expr(callee, env, subst, vargen, arities)?;
            let mut targs = Vec::with_capacity(args.len());
            for a in args {
                targs.push(infer_expr(a, env, subst, vargen, arities)?);
            }
            let ret = vargen.fresh();
            let arg_types = targs.iter().map(|t| t.ty.clone()).collect();
            let expected = Type::Fun(arg_types, Box::new(ret.clone()));
            unify(subst, &callee_t.ty, &expected)?;
            let ty = apply(subst, &ret);
            (TExprKind::Call(Box::new(callee_t), targs), ty)
        }
        // ブロックは新しいスコープ。親の型環境をクローンして始め、抜けると破棄する。
        Expr::Block(stmts) => {
            let mut block_env = env.clone();
            let (tstmts, ty) = infer_stmts(stmts, &mut block_env, subst, vargen, arities)?;
            (TExprKind::Block(tstmts), ty)
        }
        // パターンマッチ: 主語の型に各腕のパターン型を unify、全腕の本体型を互いに unify。
        Expr::Case(subject, arms) => {
            let st = infer_expr(subject, env, subst, vargen, arities)?;
            let result = vargen.fresh();
            let mut tarms = Vec::with_capacity(arms.len());
            for (pattern, body) in arms {
                let (pty, bindings) = infer_pattern(pattern, env, subst, vargen)?;
                // パターンが主語と同じ型を覆うこと。
                unify(subst, &pty, &st.ty)?;
                // パターンが束縛した変数を単相スキームで環境に入れて本体を推論する。
                let mut arm_env = env.clone();
                for (name, t) in bindings {
                    arm_env.insert(name, Scheme::mono(t));
                }
                let bty = infer_expr(body, &arm_env, subst, vargen, arities)?;
                unify(subst, &bty.ty, &result)?;
                tarms.push((pattern.clone(), bty));
            }
            let ty = apply(subst, &result);
            (TExprKind::Case(Box::new(st), tarms), ty)
        }
    };
    Ok(TExpr { ty, kind })
}

// パターンの型と、そのパターンが束縛する変数の型を推論する。
// 戻り値: (パターンが覆う値の型, 束縛 [(変数名, 型)])。
// 構築子は env のスキームを instantiate して field 型と結果型を得て、部分パターンを再帰する。
fn infer_pattern(
    pattern: &Pattern,
    env: &TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
) -> Result<(Type, Vec<(String, Type)>), AwaError> {
    match pattern {
        Pattern::Wildcard => Ok((vargen.fresh(), Vec::new())),
        Pattern::Var(name) => {
            let t = vargen.fresh();
            Ok((t.clone(), vec![(name.clone(), t)]))
        }
        Pattern::Bool(_) => Ok((Type::Bool, Vec::new())),
        Pattern::Num(_) => Ok((Type::Num, Vec::new())),
        Pattern::Ctor(name, subs) => {
            let scheme = env
                .get(name)
                .ok_or_else(|| AwaError::UnknownConstructor(name.clone()))?;
            // 構築子型を instantiate。Fun(fields)->result か、ヌル構築子なら result そのもの。
            let (field_tys, result) = match instantiate(scheme, vargen) {
                Type::Fun(params, ret) => (params, *ret),
                other => (Vec::new(), other),
            };
            if field_tys.len() != subs.len() {
                return Err(AwaError::ArityMismatch {
                    expected: field_tys.len(),
                    found: subs.len(),
                });
            }
            let mut bindings = Vec::new();
            for (sub, fty) in subs.iter().zip(field_tys.iter()) {
                let (sty, sbind) = infer_pattern(sub, env, subst, vargen)?;
                unify(subst, &sty, fty)?;
                bindings.extend(sbind);
            }
            Ok((result, bindings))
        }
        // or パターン: 全選択肢を同じ型に unify し、束縛変数の集合が一致することを確かめる
        // （本体がどの選択肢でマッチしても同じ変数を参照できるように）。
        Pattern::Or(alts) => {
            let (ty, base) = infer_pattern(&alts[0], env, subst, vargen)?;
            for alt in &alts[1..] {
                let (alt_ty, alt_binds) = infer_pattern(alt, env, subst, vargen)?;
                unify(subst, &alt_ty, &ty)?;
                // 同じ名前の集合か、同名の型は unify できるか。
                if alt_binds.len() != base.len() {
                    return Err(AwaError::OrPatternBindings);
                }
                for (name, bty) in &alt_binds {
                    match base.iter().find(|(n, _)| n == name) {
                        Some((_, base_ty)) => unify(subst, bty, base_ty)?,
                        None => return Err(AwaError::OrPatternBindings),
                    }
                }
            }
            Ok((ty, base))
        }
    }
}

// ブロック内の文の並びを推論し、(型付き文の並び, ブロックの値の型) を返す。
// 値の型は末尾が式ならその型、空 or 末尾が let/type なら Unit（eval の Block と同じ規則）。
// ブロック内の型宣言はそのブロックスコープ限りなので arity はローカルに複製して扱う。
fn infer_stmts(
    stmts: &[Stmt],
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &Arities,
) -> Result<(Vec<TStmt>, Type), AwaError> {
    let mut local_arities = arities.clone();
    let mut tstmts = Vec::with_capacity(stmts.len());
    let mut last = Type::Unit;
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, annotation, expr) => {
                let te = infer_let(name, annotation, expr, env, subst, vargen, &local_arities)?;
                tstmts.push(TStmt::Let(name.clone(), te));
                last = Type::Unit; // let 文はブロック値に寄与しない
            }
            Stmt::Type(def) => {
                register_type(def, env, subst, vargen, &mut local_arities)?;
                tstmts.push(TStmt::Type(def.clone()));
                last = Type::Unit; // 型宣言もブロック値に寄与しない
            }
            Stmt::Expr(expr) => {
                let te = infer_expr(expr, env, subst, vargen, &local_arities)?;
                last = te.ty.clone();
                tstmts.push(TStmt::Expr(te));
            }
        };
    }
    Ok((tstmts, apply(subst, &last)))
}

// 二項演算の型付け規則。3 つに分かれる:
//   算術 (+ - * /)        : 両辺 Num、結果 Num
//   比較 (< > <= >=)      : 両辺 Num、結果 Bool
//   等価 (== !=)          : 両辺は同じ型同士、結果 Bool
// eval_binop の実行時規則と対応している。
fn infer_binop(lt: &Type, op: &Token, rt: &Type, subst: &mut Subst) -> Result<Type, AwaError> {
    match op {
        Token::Plus | Token::Minus | Token::Star | Token::Slash => {
            unify(subst, lt, &Type::Num)?;
            unify(subst, rt, &Type::Num)?;
            Ok(Type::Num)
        }
        Token::Lt | Token::Gt | Token::Le | Token::Ge => {
            unify(subst, lt, &Type::Num)?;
            unify(subst, rt, &Type::Num)?;
            Ok(Type::Bool)
        }
        Token::EqEq | Token::NotEq => {
            unify(subst, lt, rt)?;
            Ok(Type::Bool)
        }
        // パーサが BinOp に入れるのは上記の演算子トークンのみ。
        other => unreachable!("二項演算子でないトークン: {:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::Parser;

    // ソースを型検査し、最後の文の型を返す簡易ヘルパ。
    fn infer(src: &str) -> Result<Type, AwaError> {
        let tokens = tokenize(src).unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let mut checker = Checker::new();
        checker.check_program(&program).map(|typed| {
            typed
                .last()
                .map(|ts| match ts {
                    TStmt::Let(_, te) | TStmt::Expr(te) => te.ty.clone(),
                    TStmt::Type(_) => Type::Unit,
                })
                .unwrap_or(Type::Unit)
        })
    }

    #[test]
    fn 算術はnum() {
        assert_eq!(infer("1 + 2 * 3"), Ok(Type::Num));
    }

    #[test]
    fn 型付きastの内部ノードにも型がつく() {
        // let f = fn(x) { x + 1 } を elaborate し、内部ノードの型を確認する。
        let tokens = tokenize("let f = fn(x) { x + 1 }").unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let mut checker = Checker::new();
        let typed = checker.check_program(&program).unwrap();
        let TStmt::Let(_, te) = &typed[0] else {
            panic!("let のはず");
        };
        // f 自体は fn(Num) -> Num。
        assert_eq!(te.ty, Type::Fun(vec![Type::Num], Box::new(Type::Num)));
        let TExprKind::Lambda(params, body) = &te.kind else {
            panic!("lambda のはず");
        };
        assert_eq!(params, &vec!["x".to_string()]);
        // 本体はブロック { x + 1 }。ブロックの値型は Num。
        assert_eq!(body.ty, Type::Num);
        let TExprKind::Block(stmts) = &body.kind else {
            panic!("block のはず");
        };
        let TStmt::Expr(binop) = &stmts[0] else {
            panic!("式文のはず");
        };
        // x + 1 も、その左辺 x も Num に解決されている。
        assert_eq!(binop.ty, Type::Num);
        let TExprKind::BinOp(left, _, _) = &binop.kind else {
            panic!("binop のはず");
        };
        assert_eq!(left.ty, Type::Num);
    }

    #[test]
    fn 比較はbool() {
        assert_eq!(infer("1 < 2"), Ok(Type::Bool));
    }

    #[test]
    fn 等価はbool() {
        assert_eq!(infer("1 == 2"), Ok(Type::Bool));
        assert_eq!(infer("true == false"), Ok(Type::Bool));
    }

    #[test]
    fn boolに算術は型エラー() {
        assert!(infer("true + 1").is_err());
    }

    #[test]
    fn 噛み合わない等価は型エラー() {
        assert!(infer("1 == true").is_err());
    }

    #[test]
    fn ifの条件はboolでなければならない() {
        assert!(infer("if 1 { 2 } else { 3 }").is_err());
    }

    #[test]
    fn ifの両腕は同じ型() {
        assert_eq!(infer("if true { 1 } else { 2 }"), Ok(Type::Num));
        assert!(infer("if true { 1 } else { false }").is_err());
    }

    #[test]
    fn 関数の型を推論する() {
        // fn(a, b) { a + b } は両引数 Num、結果 Num。
        assert_eq!(
            infer("fn(a, b) { a + b }"),
            Ok(Type::Fun(vec![Type::Num, Type::Num], Box::new(Type::Num)))
        );
    }

    #[test]
    fn 関数呼び出しの結果型() {
        assert_eq!(infer("let add = fn(a, b) { a + b }; add(1, 2)"), Ok(Type::Num));
    }

    #[test]
    fn 関数でない値の呼び出しは型エラー() {
        assert!(infer("let x = 5; x(1)").is_err());
    }

    #[test]
    fn 再帰関数の型推論() {
        assert_eq!(
            infer("let fact = fn(n) { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)"),
            Ok(Type::Num)
        );
    }

    #[test]
    fn 高階関数の型推論() {
        assert_eq!(
            infer("let inc = fn(x) { x + 1 }; let twice = fn(f, x) { f(f(x)) }; twice(inc, 0)"),
            Ok(Type::Num)
        );
    }

    #[test]
    fn ブロックの型は末尾の式() {
        assert_eq!(infer("{ let a = 2; a * 3 }"), Ok(Type::Num));
    }

    #[test]
    fn 末尾がletのブロックはunit() {
        assert_eq!(infer("{ let a = 1 }"), Ok(Type::Unit));
    }

    #[test]
    fn 未定義変数は型エラー() {
        assert_eq!(infer("q + 1"), Err(AwaError::UndefinedVariable("q".to_string())));
    }

    // ===== let 多相 =====

    #[test]
    fn 恒等関数は多相_numとboolの両方に使える() {
        // 単相 HM ではここで CannotUnify になっていた。let 多相で通るようになる。
        assert_eq!(
            infer("let id = fn(x) { x }; id(1); id(true)"),
            Ok(Type::Bool)
        );
    }

    #[test]
    fn 恒等関数を一度numに使っても他方で再利用できる() {
        assert_eq!(
            infer("let id = fn(x) { x }; let a = id(1); let b = id(false); a"),
            Ok(Type::Num)
        );
    }

    #[test]
    fn ラムダ引数は単相_numとboolの両方には使えない() {
        // f は引数なので多相化されない（同じ型変数を共有）。f(1) で引数 Num と
        // 決まった後 f(true) は Bool を要求し CannotUnify。同じ本体でも let 束縛なら
        // 多相になり通る点が対照的（恒等関数のテスト参照）。
        assert!(infer("fn(f) { f(1) + f(true) }").is_err());
    }

    #[test]
    fn 多相関数を別の多相関数に渡す() {
        // apply は ∀a b. fn(fn(a)->b, a) -> b。id を Num/Bool 双方に使う。
        assert_eq!(
            infer("let id = fn(x) { x }; let ap = fn(f, v) { f(v) }; ap(id, 1); ap(id, true)"),
            Ok(Type::Bool)
        );
    }

    // ===== ADT / パターンマッチ =====

    fn con(name: &str, args: Vec<Type>) -> Type {
        Type::Con(name.to_string(), args)
    }

    #[test]
    fn 構築子で値を作ると型はその型() {
        assert_eq!(
            infer("type Opt(a) { Some(a); None }; Some(1)"),
            Ok(con("Opt", vec![Type::Num]))
        );
    }

    #[test]
    fn caseの結果は腕の本体型() {
        assert_eq!(
            infer("type Opt(a) { Some(a); None }; case Some(1) { Some(n) -> n + 1; None -> 0 }"),
            Ok(Type::Num)
        );
    }

    #[test]
    fn option分解関数は多相() {
        // unwrap_or : fn(Opt(a), a) -> a。Bool でも使えることで多相性を確認。
        assert_eq!(
            infer(
                "type Opt(a) { Some(a); None }; \
                 let f = fn(o, d) { case o { Some(x) -> x; None -> d } }; \
                 f(Some(1), 0); f(Some(true), false)"
            ),
            Ok(Type::Bool)
        );
    }

    #[test]
    fn 再帰リストのlengthはnum() {
        assert_eq!(
            infer(
                "type List(a) { Cons(a, List(a)); Nil }; \
                 let len = fn(xs) { case xs { Nil -> 0; Cons(h, t) -> 1 + len(t) } }; \
                 len(Cons(1, Cons(2, Nil)))"
            ),
            Ok(Type::Num)
        );
    }

    #[test]
    fn 入れ子パターンを推論する() {
        assert_eq!(
            infer(
                "type List(a) { Cons(a, List(a)); Nil }; \
                 case Cons(1, Nil) { Cons(h, _) -> h; Nil -> 0 }"
            ),
            Ok(Type::Num)
        );
    }

    #[test]
    fn 主語とパターンの型が食い違うと型エラー() {
        // Num の主語に Bool 構築子のパターンを当てると CannotUnify。
        assert!(
            infer("type Opt(a) { Some(a); None }; case 1 { Some(x) -> x; None -> 0 }").is_err()
        );
    }

    #[test]
    fn 未定義の型を参照する構築子フィールド() {
        assert_eq!(
            infer("type Bad { Wrap(Ghost) }"),
            Err(AwaError::UnknownType("Ghost".to_string()))
        );
    }
}
