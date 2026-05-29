use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, Stmt};
use crate::error::AwaError;
use crate::token::Token;
use crate::types::{Scheme, Subst, Type, VarGen, apply, ftv, instantiate, unify};

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
}

impl Checker {
    pub fn new() -> Self {
        Checker::default()
    }

    // プログラム（トップレベルの文の並び）を型検査する。
    // eval_program と対になり、各文の型を順に返す（REPL 表示や検査結果用）。
    // let の束縛はトップレベルの self.env に積まれ、次の行から参照できる。
    pub fn check_program(&mut self, stmts: &[Stmt]) -> Result<Vec<Type>, AwaError> {
        let mut types = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            let ty = match stmt {
                Stmt::Let(name, expr) => {
                    let t = infer_let(name, expr, &mut self.env, &mut self.subst, &mut self.vargen)?;
                    // let 文自体の「文の値」は束縛した値そのもの（eval_program に合わせる）。
                    t
                }
                Stmt::Expr(expr) => {
                    let t = infer_expr(expr, &self.env, &mut self.subst, &mut self.vargen)?;
                    apply(&self.subst, &t)
                }
            };
            types.push(ty);
        }
        Ok(types)
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
    expr: &Expr,
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
) -> Result<Type, AwaError> {
    let placeholder = vargen.fresh();
    env.insert(name.to_string(), Scheme::mono(placeholder.clone()));
    let t = infer_expr(expr, env, subst, vargen)?;
    unify(subst, &placeholder, &t)?;
    // 自分自身の単相 placeholder を環境から外してから一般化する。
    // 外さないと placeholder が型変数を環境に縛り、何も量化されなくなる。
    env.remove(name);
    let scheme = generalize(env, subst, &placeholder);
    env.insert(name.to_string(), scheme.clone());
    Ok(scheme.ty)
}

// 式の型を推論する本体。env は読むだけ（スコープの拡張はクローンして行う）。
// subst と vargen は推論の進行に伴い更新するので可変借用で受け取る。
fn infer_expr(
    expr: &Expr,
    env: &TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
) -> Result<Type, AwaError> {
    match expr {
        Expr::Num(_) => Ok(Type::Num),
        Expr::Bool(_) => Ok(Type::Bool),
        // 変数は型環境を引き、スキームを instantiate して fresh な型に具体化する。
        // 多相な束縛はここで使用箇所ごとに独立した型変数を得る。
        // 無ければ評価時と同じ UndefinedVariable。
        Expr::Var(name) => env
            .get(name)
            .map(|scheme| instantiate(scheme, vargen))
            .ok_or_else(|| AwaError::UndefinedVariable(name.clone())),
        // 単項マイナスはオペランドが Num、結果も Num。
        Expr::Neg(operand) => {
            let t = infer_expr(operand, env, subst, vargen)?;
            unify(subst, &t, &Type::Num)?;
            Ok(Type::Num)
        }
        Expr::BinOp(left, op, right) => {
            let lt = infer_expr(left, env, subst, vargen)?;
            let rt = infer_expr(right, env, subst, vargen)?;
            infer_binop(&lt, op, &rt, subst)
        }
        // if: 条件は Bool、両腕は同じ型でなければならない。結果はその腕の型。
        Expr::If(cond, then_branch, else_branch) => {
            let ct = infer_expr(cond, env, subst, vargen)?;
            unify(subst, &ct, &Type::Bool)?;
            let tt = infer_expr(then_branch, env, subst, vargen)?;
            let et = infer_expr(else_branch, env, subst, vargen)?;
            unify(subst, &tt, &et)?;
            Ok(apply(subst, &tt))
        }
        // 無名関数: 各引数に fresh 型変数を割り当て、その下で本体を推論する。
        // 結果は fn(引数型...) -> 本体型。置換を反映した最終形にして返す。
        Expr::Lambda(params, body) => {
            let mut body_env = env.clone();
            let mut param_types = Vec::with_capacity(params.len());
            for p in params {
                let tv = vargen.fresh();
                // ラムダ引数は単相（多相化しない）。HM の多相境界は let だけ。
                body_env.insert(p.clone(), Scheme::mono(tv.clone()));
                param_types.push(tv);
            }
            let ret = infer_expr(body, &body_env, subst, vargen)?;
            let param_types = param_types.iter().map(|t| apply(subst, t)).collect();
            Ok(Type::Fun(param_types, Box::new(apply(subst, &ret))))
        }
        // 呼び出し: callee を fn(実引数型...) -> 新しい戻り型 と unify する。
        // unify が「callee が関数か」「引数の個数と型が合うか」を一括で検査する。
        // 戻り型は fresh 型変数で置き、unify 後に確定したものを返す。
        Expr::Call(callee, args) => {
            let callee_t = infer_expr(callee, env, subst, vargen)?;
            let mut arg_types = Vec::with_capacity(args.len());
            for a in args {
                arg_types.push(infer_expr(a, env, subst, vargen)?);
            }
            let ret = vargen.fresh();
            let expected = Type::Fun(arg_types, Box::new(ret.clone()));
            unify(subst, &callee_t, &expected)?;
            Ok(apply(subst, &ret))
        }
        // ブロックは新しいスコープ。親の型環境をクローンして始め、抜けると破棄する。
        Expr::Block(stmts) => {
            let mut block_env = env.clone();
            infer_stmts(stmts, &mut block_env, subst, vargen)
        }
    }
}

// ブロック内の文の並びを推論し、ブロックの値の型を返す。
// 末尾が式ならその型、空 or 末尾が let なら Unit（eval の Block と同じ規則）。
fn infer_stmts(
    stmts: &[Stmt],
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
) -> Result<Type, AwaError> {
    let mut last = Type::Unit;
    for stmt in stmts {
        last = match stmt {
            Stmt::Let(name, expr) => {
                infer_let(name, expr, env, subst, vargen)?;
                Type::Unit // let 文はブロック値に寄与しない
            }
            Stmt::Expr(expr) => infer_expr(expr, env, subst, vargen)?,
        };
    }
    Ok(apply(subst, &last))
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
        checker
            .check_program(&program)
            .map(|types| types.last().cloned().unwrap_or(Type::Unit))
    }

    #[test]
    fn 算術はnum() {
        assert_eq!(infer("1 + 2 * 3"), Ok(Type::Num));
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
}
