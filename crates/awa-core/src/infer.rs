use std::collections::{HashMap, HashSet};

use crate::ast::{
    Expr, ExprKind, ImplBlock, Pattern, Stmt, TExpr, TExprKind, TMethod, TStmt, TypeDef, TypeExpr,
    TypeExprKind,
};
use crate::error::{AwaError, LocatedError};
use crate::match_check::{Sigs, check_matches};
use crate::span::Span;
use crate::token::Token;
use crate::types::{Scheme, Subst, Type, VarGen, apply, ftv, instantiate, render_type, unify};

use std::cell::RefCell;

thread_local! {
    // レコード型名（解決済み・prefix 付き）→ フィールド宣言順。check_program 開始時に全 record を
    // 集めておき、infer_expr の `p.x` アクセスと `{ p | .. }` 更新の脱糖で参照する。
    // 引数で全 infer 関数へ通すと配管が膨大なので thread_local にした（infer は単一スレッド実行）。
    static RECORD_FIELDS: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

// レコード型名（解決済み）→ フィールド宣言順を引く。
fn record_order(name: &str) -> Option<Vec<String>> {
    RECORD_FIELDS.with(|r| r.borrow().get(name).cloned())
}

// 名前付きフィールド（field, T）を宣言順 order に並べ替える。重複・未知・不足を検査。
fn reorder_named<T>(ty: &str, order: &[String], named: Vec<(String, T)>) -> Result<Vec<T>, LocatedError> {
    let mut slots: Vec<Option<T>> = Vec::with_capacity(order.len());
    for _ in 0..order.len() {
        slots.push(None);
    }
    for (fname, val) in named {
        match order.iter().position(|f| f == &fname) {
            Some(i) => {
                if slots[i].is_some() {
                    return Err(LocatedError::from(AwaError::DuplicateField(fname)));
                }
                slots[i] = Some(val);
            }
            None => {
                return Err(LocatedError::from(AwaError::UnknownField {
                    ty: ty.to_string(),
                    field: fname,
                }));
            }
        }
    }
    let missing: Vec<String> = order
        .iter()
        .zip(slots.iter())
        .filter(|(_, s)| s.is_none())
        .map(|(f, _)| f.clone())
        .collect();
    if !missing.is_empty() {
        return Err(LocatedError::from(AwaError::MissingFields {
            ty: ty.to_string(),
            fields: missing.join(", "),
        }));
    }
    Ok(slots.into_iter().map(|s| s.unwrap()).collect())
}

// ===== レコード脱糖パス（全実行経路の check_program 冒頭で走る）=====
// RecordLit Name { f = e } → 宣言順に並べた構築子呼び出し Call(Var(Name), [..])。
// レコードパターン Name { f = p } → 構築子パターン Ctor(Name, [..])。
// これで infer/match_check/eval/codegen はレコードリテラル/パターンを一切見ない。
// `p.x` アクセスと `{ p | .. }` 更新は型が要るので infer_expr で別途脱糖する（ここでは子を辿るだけ）。
fn desugar_records_stmts(stmts: &[Stmt]) -> Result<Vec<Stmt>, LocatedError> {
    stmts.iter().map(dsr_stmt).collect()
}

fn dsr_stmt(s: &Stmt) -> Result<Stmt, LocatedError> {
    Ok(match s {
        Stmt::Let(n, ann, e) => Stmt::Let(n.clone(), ann.clone(), dsr_expr(e)?),
        Stmt::Expr(e) => Stmt::Expr(dsr_expr(e)?),
        Stmt::Type(d) => Stmt::Type(d.clone()),
        Stmt::Impl(b) => {
            let mut b = b.clone();
            for m in &mut b.methods {
                m.body = dsr_expr(&m.body)?;
            }
            Stmt::Impl(b)
        }
    })
}

fn dsr_pattern(p: &Pattern) -> Result<Pattern, LocatedError> {
    Ok(match p {
        Pattern::Ctor(name, subs) => {
            Pattern::Ctor(name.clone(), subs.iter().map(dsr_pattern).collect::<Result<_, _>>()?)
        }
        Pattern::Or(alts) => Pattern::Or(alts.iter().map(dsr_pattern).collect::<Result<_, _>>()?),
        Pattern::Array(subs) => {
            Pattern::Array(subs.iter().map(dsr_pattern).collect::<Result<_, _>>()?)
        }
        Pattern::Tuple(subs) => {
            Pattern::Tuple(subs.iter().map(dsr_pattern).collect::<Result<_, _>>()?)
        }
        Pattern::Record(name, fields) => {
            let order = record_order(name)
                .ok_or_else(|| LocatedError::from(AwaError::NotARecord(name.clone())))?;
            let named: Vec<(String, Pattern)> = fields
                .iter()
                .map(|(f, p)| Ok::<_, LocatedError>((f.clone(), dsr_pattern(p)?)))
                .collect::<Result<_, _>>()?;
            Pattern::Ctor(name.clone(), reorder_named(name, &order, named)?)
        }
        other => other.clone(),
    })
}

fn dsr_expr(e: &Expr) -> Result<Expr, LocatedError> {
    let kind = match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Long(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Var(_) => e.kind.clone(),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(dsr_expr(x)?)),
        ExprKind::BinOp(l, op, r) => {
            ExprKind::BinOp(Box::new(dsr_expr(l)?), op.clone(), Box::new(dsr_expr(r)?))
        }
        ExprKind::Lambda(ps, ret, body) => {
            ExprKind::Lambda(ps.clone(), ret.clone(), Box::new(dsr_expr(body)?))
        }
        ExprKind::Call(c, args) => ExprKind::Call(
            Box::new(dsr_expr(c)?),
            args.iter().map(dsr_expr).collect::<Result<_, _>>()?,
        ),
        ExprKind::Block(stmts) => ExprKind::Block(desugar_records_stmts(stmts)?),
        ExprKind::Case(subj, arms) => {
            let subj = Box::new(dsr_expr(subj)?);
            let arms = arms
                .iter()
                .map(|(p, b)| Ok::<_, LocatedError>((dsr_pattern(p)?, dsr_expr(b)?)))
                .collect::<Result<_, _>>()?;
            ExprKind::Case(subj, arms)
        }
        ExprKind::MethodCall(recv, m, args) => ExprKind::MethodCall(
            Box::new(dsr_expr(recv)?),
            m.clone(),
            args.iter().map(dsr_expr).collect::<Result<_, _>>()?,
        ),
        ExprKind::Field(recv, m) => ExprKind::Field(Box::new(dsr_expr(recv)?), m.clone()),
        ExprKind::ArrayLit(xs) => {
            ExprKind::ArrayLit(xs.iter().map(dsr_expr).collect::<Result<_, _>>()?)
        }
        ExprKind::TupleLit(xs) => {
            ExprKind::TupleLit(xs.iter().map(dsr_expr).collect::<Result<_, _>>()?)
        }
        // レコードリテラル → 構築子呼び出し（宣言順）。
        ExprKind::RecordLit(name, inits) => {
            let order = record_order(name)
                .ok_or_else(|| LocatedError::from(AwaError::NotARecord(name.clone())))?;
            let named: Vec<(String, Expr)> = inits
                .iter()
                .map(|(f, ie)| Ok::<_, LocatedError>((f.clone(), dsr_expr(ie)?)))
                .collect::<Result<_, _>>()?;
            let args = reorder_named(name, &order, named)?;
            let callee = Expr { kind: ExprKind::Var(name.clone()), span: e.span };
            ExprKind::Call(Box::new(callee), args)
        }
        // 更新は型が要るので infer_expr で脱糖。ここでは子だけ辿る。
        ExprKind::RecordUpdate(base, inits) => ExprKind::RecordUpdate(
            Box::new(dsr_expr(base)?),
            inits
                .iter()
                .map(|(f, ie)| Ok::<_, LocatedError>((f.clone(), dsr_expr(ie)?)))
                .collect::<Result<_, _>>()?,
        ),
    };
    Ok(Expr { kind, span: e.span })
}

// 型名 → 型パラメータの個数（arity）。型式の解決時に「Option(a) の引数個数が
// 宣言と合うか」を検査するのに使う。3b の網羅性チェックで variant 一覧へ拡張予定。
type Arities = HashMap<String, usize>;

// メソッド表: (型頭名, メソッド名) → メソッドの関数型スキーム（self を第一引数に取る）。
type MethodEnv = HashMap<(String, String), Scheme>;

// 値の型から「型頭名」を取り出す（メソッド解決のキー）。関数型・型変数は名前を持たない。
fn type_head(ty: &Type) -> Option<String> {
    match ty {
        Type::Con(name, _) => Some(name.clone()),
        Type::Int => Some("Int".to_string()),
        Type::Long => Some("Long".to_string()),
        Type::Float => Some("Float".to_string()),
        Type::Str => Some("String".to_string()),
        Type::Bool => Some("Bool".to_string()),
        Type::Unit => Some("Nil".to_string()),
        Type::Fun(..) | Type::Var(_) => None,
    }
}

// AwaError（種類）に span を添えて LocatedError にする小ヘルパ。
// unify などの失敗を「その式の位置」で包むのに使う。
fn at<T>(r: Result<T, AwaError>, span: Span) -> Result<T, LocatedError> {
    r.map_err(|e| LocatedError::at(e, span))
}

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
    // メソッド表（impl で登録）。REPL で行をまたいで持ち越す。
    methods: MethodEnv,
    // レコード型名 → フィールド宣言順。REPL で行をまたいで持ち越す（脱糖に使う）。
    // infer_expr など free 関数からは RECORD_FIELDS thread_local 経由で読む。
    record_fields: HashMap<String, Vec<String>>,
}

impl Checker {
    pub fn new() -> Self {
        let mut c = Checker::default();
        // 組込関数（変換）の型スキームを seed する。eval::builtins_env と名前を一致させること。
        let mono = |params: Vec<Type>, ret: Type| Scheme::mono(Type::Fun(params, Box::new(ret)));
        let builtins: [(&str, Type, Type); 6] = [
            ("int_to_float", Type::Int, Type::Float),
            ("float_to_int", Type::Float, Type::Int),
            ("int_to_long", Type::Int, Type::Long),
            ("long_to_int", Type::Long, Type::Int),
            ("int_to_string", Type::Int, Type::Str),
            ("float_to_string", Type::Float, Type::Str),
        ];
        for (name, arg, ret) in builtins {
            c.env.insert(name.to_string(), mono(vec![arg], ret));
        }
        c.seed_async();
        c
    }

    // 非同期の組込型・関数・メソッドを seed する（eval::builtins_env と名前を一致させること）。
    // Future は組込型（arity 1）。pure/delay/run は関数、flatMap/map は Future のメソッド。
    fn seed_async(&mut self) {
        self.arities.insert("Future".to_string(), 1);
        // 新しい型変数 Type::Var(id) と、その id を返す。
        let mut fresh = || match self.vargen.fresh() {
            Type::Var(id) => id,
            _ => unreachable!(),
        };
        let future = |t: Type| Type::Con("Future".to_string(), vec![t]);
        let scheme = |vars: Vec<u32>, params: Vec<Type>, ret: Type| Scheme {
            vars,
            ty: Type::Fun(params, Box::new(ret)),
        };

        // pure : ∀a. a -> Future(a)
        let a = fresh();
        self.env.insert(
            "pure".to_string(),
            scheme(vec![a], vec![Type::Var(a)], future(Type::Var(a))),
        );
        // delay : ∀a. (Int, a) -> Future(a)
        let a = fresh();
        self.env.insert(
            "delay".to_string(),
            scheme(vec![a], vec![Type::Int, Type::Var(a)], future(Type::Var(a))),
        );
        // run : ∀a. Future(a) -> a
        let a = fresh();
        self.env.insert(
            "run".to_string(),
            scheme(vec![a], vec![future(Type::Var(a))], Type::Var(a)),
        );
        // Future(a).flatMap : ∀a b. (Future(a), fn(a) -> Future(b)) -> Future(b)
        let (a, b) = (fresh(), fresh());
        let k = Type::Fun(vec![Type::Var(a)], Box::new(future(Type::Var(b))));
        self.methods.insert(
            ("Future".to_string(), "flatMap".to_string()),
            scheme(vec![a, b], vec![future(Type::Var(a)), k], future(Type::Var(b))),
        );
        // Future(a).map : ∀a b. (Future(a), fn(a) -> b) -> Future(b)
        let (a, b) = (fresh(), fresh());
        let k = Type::Fun(vec![Type::Var(a)], Box::new(Type::Var(b)));
        self.methods.insert(
            ("Future".to_string(), "map".to_string()),
            scheme(vec![a, b], vec![future(Type::Var(a)), k], future(Type::Var(b))),
        );

        // OS 連携（同期 I/O・時間）。すべて単相。
        let mono = |params: Vec<Type>, ret: Type| Scheme::mono(Type::Fun(params, Box::new(ret)));
        self.env.insert("print".to_string(), mono(vec![Type::Str], Type::Unit));
        self.env.insert("read_line".to_string(), mono(vec![], Type::Str));
        self.env.insert("read_file".to_string(), mono(vec![Type::Str], Type::Str));
        self.env.insert("write_file".to_string(), mono(vec![Type::Str, Type::Str], Type::Unit));
        self.env.insert("now".to_string(), mono(vec![], Type::Int));
        self.env.insert("string_to_int".to_string(), mono(vec![Type::Str], Type::Int));
        self.env.insert("string_length".to_string(), mono(vec![Type::Str], Type::Int));
        self.env.insert("string_byte_length".to_string(), mono(vec![Type::Str], Type::Int));
        // 数学関数（libm 委譲。std/math がラップする）。単項は Float -> Float。
        for name in [
            "math_sqrt", "math_sin", "math_cos", "math_tan", "math_asin", "math_acos", "math_atan",
            "math_exp", "math_ln", "math_log10", "math_floor", "math_ceil", "math_round",
            "math_cbrt", "math_trunc", "math_log2", "math_sinh", "math_cosh", "math_tanh",
        ] {
            self.env.insert(name.to_string(), mono(vec![Type::Float], Type::Float));
        }
        self.env.insert("math_pow".to_string(), mono(vec![Type::Float, Type::Float], Type::Float));
        self.env.insert("math_atan2".to_string(), mono(vec![Type::Float, Type::Float], Type::Float));
        self.env.insert("math_hypot".to_string(), mono(vec![Type::Float, Type::Float], Type::Float));
        // 文字単位の操作（JSON/HTTP パーサを awa で書くための土台）。
        // string_char_at: i 番目のコードポイント（範囲外は -1）。char_to_string: 1 文字 String。
        self.env.insert("string_char_at".to_string(), mono(vec![Type::Str, Type::Int], Type::Int));
        self.env.insert("char_to_string".to_string(), mono(vec![Type::Int], Type::Str));
        self.env.insert("string_to_float".to_string(), mono(vec![Type::Str], Type::Float));

        // OS 連携（ソケット非同期）。Socket は組込型（fd ハンドル、arity 0）。native ビルド専用。
        self.arities.insert("Socket".to_string(), 0);
        let socket = Type::Con("Socket".to_string(), vec![]);
        let fut = |t: Type| Type::Con("Future".to_string(), vec![t]);
        self.env.insert("tcp_listen".to_string(), mono(vec![Type::Int], socket.clone()));
        self.env.insert("sock_port".to_string(), mono(vec![socket.clone()], Type::Int));
        self.env.insert("accept".to_string(), mono(vec![socket.clone()], fut(socket.clone())));
        self.env.insert("sock_read".to_string(), mono(vec![socket.clone()], fut(Type::Str)));
        self.env.insert("sock_write".to_string(), mono(vec![socket.clone(), Type::Str], fut(Type::Unit)));
        self.env.insert("sock_close".to_string(), mono(vec![socket.clone()], Type::Unit));
        self.env.insert("tcp_connect".to_string(), mono(vec![Type::Str, Type::Int], fut(socket)));
        // ファイル擬似非同期（native はスレッドプール、インタプリタは同期読み→即解決 Future）。
        self.env.insert("read_file_async".to_string(), mono(vec![Type::Str], fut(Type::Str)));

        // ===== 配列プリミティブ（不変・永続 Array(a)）=====
        // std/array が at→Option・map/filter/fold などの高水準 API を組む土台。すべて多相。
        self.arities.insert("Array".to_string(), 1);
        let array = |t: Type| Type::Con("Array".to_string(), vec![t]);
        // 多相プリミティブごとに新しい型変数 a を割り当てて scheme を作る。
        let mut afresh = || match self.vargen.fresh() {
            Type::Var(id) => id,
            _ => unreachable!(),
        };
        // array_empty : ∀a. () -> Array(a)
        let a = afresh();
        self.env.insert(
            "array_empty".to_string(),
            scheme(vec![a], vec![], array(Type::Var(a))),
        );
        // array_length : ∀a. Array(a) -> Int
        let a = afresh();
        self.env.insert(
            "array_length".to_string(),
            scheme(vec![a], vec![array(Type::Var(a))], Type::Int),
        );
        // array_get : ∀a. (Array(a), Int) -> a（無検査。std/array.at が範囲を守る）
        let a = afresh();
        self.env.insert(
            "array_get".to_string(),
            scheme(vec![a], vec![array(Type::Var(a)), Type::Int], Type::Var(a)),
        );
        // array_set : ∀a. (Array(a), Int, a) -> Array(a)（永続更新）
        let a = afresh();
        self.env.insert(
            "array_set".to_string(),
            scheme(
                vec![a],
                vec![array(Type::Var(a)), Type::Int, Type::Var(a)],
                array(Type::Var(a)),
            ),
        );
        // array_push : ∀a. (Array(a), a) -> Array(a)（永続更新）
        let a = afresh();
        self.env.insert(
            "array_push".to_string(),
            scheme(vec![a], vec![array(Type::Var(a)), Type::Var(a)], array(Type::Var(a))),
        );
    }

    // プログラム（トップレベルの文の並び）を型検査し、型付き AST（elaboration の結果）を返す。
    // eval_program はこの型付きプログラムを実行する。let の束縛・型宣言はトップレベルの
    // self に積まれ、次の行から参照できる。型宣言は値文を生まない（eval と整合）。
    pub fn check_program(&mut self, stmts: &[Stmt]) -> Result<Vec<TStmt>, LocatedError> {
        // レコードのフィールド順を Checker に蓄積（REPL で行をまたいで持ち越す）。前方参照や
        // 宣言前に使う関数のため、本体推論の前に今回の全 record を集める。
        for stmt in stmts {
            if let Stmt::Type(def) = stmt {
                if let Some(fields) = &def.record_fields {
                    self.record_fields.insert(def.name.clone(), fields.clone());
                }
            }
        }
        // この Checker の蓄積を thread_local へ反映（毎回入れ替え＝他検査の残留を排除）。
        RECORD_FIELDS.with(|r| *r.borrow_mut() = self.record_fields.clone());
        // レコードリテラル/パターンを構築子呼び出し/パターンへ脱糖（全実行経路で共通）。
        let stmts = desugar_records_stmts(stmts)?;
        let stmts = &stmts[..];
        let mut typed = Vec::new();
        // 相互再帰: トップレベルの関数 let を、本体を推論する前に一括で pre-declare する。
        // これで定義順に関係なく互いを参照できる（注釈があればその型、無ければ fresh）。
        // プレースホルダは fresh 変数のみ（注釈はまだ解決しない＝この時点では型未登録なため）。
        // 注釈との突き合わせは infer_let が本体推論時に行い、この fresh var と unify する。
        let mut prebound: HashMap<String, Type> = HashMap::new();
        for stmt in stmts {
            if let Stmt::Let(name, _, expr) = stmt {
                if matches!(expr.kind, ExprKind::Lambda(..)) {
                    let ph = self.vargen.fresh();
                    self.env.insert(name.clone(), Scheme::mono(ph.clone()));
                    prebound.insert(name.clone(), ph);
                }
            }
        }
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
                        &self.methods,
                        prebound.get(name).cloned(),
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
                        &self.methods,
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
                Stmt::Impl(def) => {
                    let tmethods = register_impl(
                        def,
                        &self.env,
                        &mut self.subst,
                        &mut self.vargen,
                        &self.arities,
                        &mut self.methods,
                    )?;
                    typed.push(TStmt::Impl(tmethods));
                }
            }
        }
        // 型検査が通ったら、全 case 式の網羅性・到達不能を静的に検査する（untyped AST 上で）。
        check_matches(stmts, &self.sigs)?;
        // 最終的な置換を全ノードの型に適用して解決する（推論中は未確定変数が残るため）。
        for ts in &mut typed {
            resolve_tstmt(ts, &self.subst);
        }
        // 解決後に数値性を確認する（算術/比較のオペランドが Int/Long/Float か。遅延チェック）。
        for ts in &typed {
            check_numeric_stmt(ts)?;
        }
        Ok(typed)
    }
}

// 型付き AST の全ノードの型に最終置換を適用して解決する（elaboration の仕上げ）。
fn resolve_texpr(te: &mut TExpr, subst: &Subst) {
    te.ty = apply(subst, &te.ty);
    match &mut te.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_)
        | TExprKind::Var(_) => {}
        TExprKind::Neg(x) => resolve_texpr(x, subst),
        TExprKind::BinOp(l, _, r) => {
            resolve_texpr(l, subst);
            resolve_texpr(r, subst);
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
        TExprKind::ArrayLit(elems) | TExprKind::TupleLit(elems) => {
            for e in elems {
                resolve_texpr(e, subst);
            }
        }
    }
}

fn resolve_tstmt(ts: &mut TStmt, subst: &Subst) {
    match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => resolve_texpr(te, subst),
        TStmt::Type(_) => {}
        TStmt::Impl(methods) => {
            for m in methods {
                resolve_texpr(&mut m.body, subst);
                for t in &mut m.param_tys {
                    *t = apply(subst, t);
                }
                m.ret = apply(subst, &m.ret);
            }
        }
    }
}

// 数値性の遅延チェック（推論・解決後の型付き AST を走査）。算術 (+,-,*,/) と単項マイナス、
// 比較 (<,>,<=,>=) のオペランド型が Int/Long/Float であることを確認する。未確定なら
// AmbiguousNumeric（注釈を要求）、数値でない具体型なら NotNumeric。即時でなく後段で見るのは、
// do の束縛変数のように「後から型が決まる」場合の誤判定を避けるため。
fn check_numeric_stmt(ts: &TStmt) -> Result<(), LocatedError> {
    match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => check_numeric(te),
        TStmt::Type(_) => Ok(()),
        TStmt::Impl(methods) => methods.iter().try_for_each(|m| check_numeric(&m.body)),
    }
}

fn check_numeric(te: &TExpr) -> Result<(), LocatedError> {
    match &te.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_)
        | TExprKind::Var(_) => Ok(()),
        TExprKind::Neg(x) => {
            at(require_numeric(&x.ty), x.span)?;
            check_numeric(x)
        }
        TExprKind::BinOp(l, op, r) => {
            // 算術・比較はオペランドが数値であること。連結 <> と等価 == は対象外。
            if matches!(
                op,
                Token::Plus
                    | Token::Minus
                    | Token::Star
                    | Token::Slash
                    | Token::Lt
                    | Token::Gt
                    | Token::Le
                    | Token::Ge
            ) {
                at(require_numeric(&l.ty), l.span)?;
            }
            check_numeric(l)?;
            check_numeric(r)
        }
        TExprKind::Lambda(_, body) => check_numeric(body),
        TExprKind::Call(callee, args) => {
            check_numeric(callee)?;
            args.iter().try_for_each(check_numeric)
        }
        TExprKind::Block(stmts) => stmts.iter().try_for_each(check_numeric_stmt),
        TExprKind::Case(subject, arms) => {
            check_numeric(subject)?;
            arms.iter().try_for_each(|(_, b)| check_numeric(b))
        }
        TExprKind::ArrayLit(elems) | TExprKind::TupleLit(elems) => {
            elems.iter().try_for_each(check_numeric)
        }
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
) -> Result<(), LocatedError> {
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

// impl ブロックを処理: 各メソッドを「self を第一引数に取る関数」として型検査し、
// (型頭, メソッド名) のスキームをメソッド表に登録する。自己再帰メソッドのため、先に
// 全メソッドの暫定シグネチャを登録してから本体を検査する（let-rec と同じ）。
// 型付き TMethod（マングル名・引数名・本体）を返す。
fn register_impl(
    def: &ImplBlock,
    env: &TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &Arities,
    methods: &mut MethodEnv,
) -> Result<Vec<TMethod>, LocatedError> {
    // self の型: 組込なら Num/Bool/Unit、ADT なら Con(name, fresh params)。
    let param_vars: Vec<Type> = def.params.iter().map(|_| vargen.fresh()).collect();
    let self_ty = resolve_con(&def.type_name, param_vars.clone(), arities)?;
    let head = type_head(&self_ty).expect("impl 対象は型頭を持つ");
    // 注釈の型変数を impl の型パラメータと共有させる土台（fn(a)->b の a が Option(a) の a）。
    let param_map: HashMap<String, Type> = def
        .params
        .iter()
        .cloned()
        .zip(param_vars.iter().cloned())
        .collect();

    // (1) 暫定シグネチャ（self_ty を第一引数とする関数型）を先に全登録。
    let mut sigs: Vec<(Vec<Type>, Type)> = Vec::with_capacity(def.methods.len());
    for m in &def.methods {
        let mut sig_vars = param_map.clone();
        let mut param_tys = vec![self_ty.clone()]; // params[0] = self
        for (_, ann) in m.params.iter().skip(1) {
            let t = match ann {
                Some(a) => resolve_annotation(a, &mut sig_vars, vargen, arities)?,
                None => vargen.fresh(),
            };
            param_tys.push(t);
        }
        let ret = match &m.ret {
            Some(a) => resolve_annotation(a, &mut sig_vars, vargen, arities)?,
            None => vargen.fresh(),
        };
        methods.insert(
            (head.clone(), m.name.clone()),
            Scheme::mono(Type::Fun(param_tys.clone(), Box::new(ret.clone()))),
        );
        sigs.push((param_tys, ret));
    }

    // (2) 各本体を検査（self＋引数を環境に入れて）。本体内の自己メソッド呼び出しは
    //     暫定スキームで解決される。
    let mut tmethods = Vec::with_capacity(def.methods.len());
    for (m, (param_tys, ret_ty)) in def.methods.iter().zip(sigs.iter()) {
        let mut body_env = env.clone();
        let mut param_names = Vec::with_capacity(m.params.len());
        for ((pname, _), pty) in m.params.iter().zip(param_tys.iter()) {
            body_env.insert(pname.clone(), Scheme::mono(pty.clone()));
            param_names.push(pname.clone());
        }
        let body_t = infer_expr(&m.body, &body_env, subst, vargen, arities, &*methods)?;
        at(unify(subst, &body_t.ty, ret_ty), m.body.span)?;
        tmethods.push(TMethod {
            mangled: format!("{}#{}", head, m.name),
            params: param_names,
            param_tys: param_tys.clone(),
            ret: ret_ty.clone(),
            body: body_t,
        });
    }

    // (3) 確定した型を generalize してメソッド表を上書き（多相メソッド: ∀a. fn(Option(a), ...)）。
    for (m, (param_tys, ret_ty)) in def.methods.iter().zip(sigs.iter()) {
        let fn_ty = Type::Fun(param_tys.clone(), Box::new(ret_ty.clone()));
        let scheme = generalize(env, subst, &fn_ty);
        methods.insert((head.clone(), m.name.clone()), scheme);
    }
    Ok(tmethods)
}

// 型構築子名（と解決済み引数）を意味論的な Type へ。組込型 Num/Bool/Nil と
// 宣言済みの型名（arity 検査つき）を扱う。型定義・注釈の両方から使う共通部分。
fn resolve_con(name: &str, args: Vec<Type>, arities: &Arities) -> Result<Type, AwaError> {
    match name {
        "Int" if args.is_empty() => Ok(Type::Int),
        "Long" if args.is_empty() => Ok(Type::Long),
        "Float" if args.is_empty() => Ok(Type::Float),
        "String" if args.is_empty() => Ok(Type::Str),
        "Bool" if args.is_empty() => Ok(Type::Bool),
        "Nil" if args.is_empty() => Ok(Type::Unit),
        // タプルは可変アリティの組込型（要素数ぶんの型引数を持つ）。
        "Tuple" => Ok(Type::Con("Tuple".to_string(), args)),
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
) -> Result<Type, LocatedError> {
    match &texpr.kind {
        TypeExprKind::Var(name) => param_map
            .get(name)
            .cloned()
            .ok_or_else(|| LocatedError::at(AwaError::UnknownType(name.clone()), texpr.span)),
        TypeExprKind::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_type_expr(a, param_map, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities).map_err(|e| LocatedError::at(e, texpr.span))
        }
        TypeExprKind::Fun(params, ret) => {
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
) -> Result<Type, LocatedError> {
    match &texpr.kind {
        TypeExprKind::Var(name) => {
            Ok(vars.entry(name.clone()).or_insert_with(|| vargen.fresh()).clone())
        }
        TypeExprKind::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_annotation(a, vars, vargen, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities).map_err(|e| LocatedError::at(e, texpr.span))
        }
        TypeExprKind::Fun(params, ret) => {
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
) -> Result<Type, LocatedError> {
    match &texpr.kind {
        TypeExprKind::Var(name) => Ok(sk
            .entry(name.clone())
            .or_insert_with(|| match vargen.fresh() {
                Type::Var(id) => Type::Con(format!("#sk{}", id), Vec::new()),
                _ => unreachable!(),
            })
            .clone()),
        TypeExprKind::Con(name, args) => {
            let resolved = args
                .iter()
                .map(|a| resolve_skolem(a, sk, vargen, arities))
                .collect::<Result<_, _>>()?;
            resolve_con(name, resolved, arities).map_err(|e| LocatedError::at(e, texpr.span))
        }
        TypeExprKind::Fun(params, ret) => {
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
#[allow(clippy::too_many_arguments)]
fn infer_let(
    name: &str,
    annotation: &Option<TypeExpr>,
    expr: &Expr,
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
    arities: &Arities,
    methods: &MethodEnv,
    // 相互再帰グループで事前宣言済みのプレースホルダ型（あればそれを再帰束縛に使う）。
    prebound: Option<Type>,
) -> Result<TExpr, LocatedError> {
    // 軽量な双方向推論: `let f: fn(A)->B = fn(x){..}` のとき、注釈の引数/戻り型を
    // ラムダへ事前注入する（未注釈の引数のみ）。これで本体推論の時点で引数型が確定し、
    // `f.flatMap(..)` のようにメソッドのレシーバ型が分かる（let 注釈だけで済む）。
    let expr_owned: Expr = match (annotation, &expr.kind) {
        (Some(TypeExpr { kind: TypeExprKind::Fun(ptys, rty), .. }), ExprKind::Lambda(params, lret, body))
            if params.len() == ptys.len() =>
        {
            let new_params = params
                .iter()
                .zip(ptys.iter())
                .map(|((n, a), pty)| (n.clone(), a.clone().or_else(|| Some(pty.clone()))))
                .collect();
            let new_ret = lret.clone().or_else(|| Some((**rty).clone()));
            Expr {
                kind: ExprKind::Lambda(new_params, new_ret, body.clone()),
                span: expr.span,
            }
        }
        _ => expr.clone(),
    };
    let expr = &expr_owned;

    // 再帰の placeholder。注釈があればその型を使う（再帰呼び出しが宣言された型＝戻り型を
    // 持てるので、`all(rest).flatMap(..)` のようにメソッドのレシーバ型が確定する）。
    // 注釈が無ければ従来どおり fresh 型変数（単相 placeholder）。
    let ann_ty = match annotation {
        Some(ann) => {
            let mut vars = HashMap::new();
            Some(resolve_annotation(ann, &mut vars, vargen, arities)?)
        }
        None => None,
    };
    // プレースホルダ（再帰呼び出しが見る自分の型）の決定。
    //  - 事前宣言の fresh var（prebound）があれば、他のトップレベル関数が既にそれへ制約を
    //    付けているので「同一性」を保つ必要がある。注釈型があればその fresh var と unify して
    //    両者を結びつけ、注釈型をプレースホルダに使う（再帰呼び出しが宣言型を持てる利点を維持）。
    //  - prebound が無ければ従来どおり 注釈型 or fresh。
    let placeholder = match (&prebound, &ann_ty) {
        (Some(p), Some(a)) => {
            at(unify(subst, p, a), expr.span)?;
            a.clone()
        }
        (Some(p), None) => p.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => vargen.fresh(),
    };
    env.insert(name.to_string(), Scheme::mono(placeholder.clone()));
    let te = infer_expr(expr, env, subst, vargen, arities, methods)?;
    let span = te.span;
    at(unify(subst, &placeholder, &te.ty), span)?;
    // 自分自身の単相 placeholder を環境から外してから一般化する。
    // 外さないと placeholder が型変数を環境に縛り、何も量化されなくなる。
    env.remove(name);
    // 型注釈があれば突き合わせる。エラーは束縛値の式 span に寄せる。
    if let Some(ann) = annotation {
        // (1) 制約: 注釈型と推論型を unify（placeholder は既に注釈型なので再確認）。
        let ann_ty = ann_ty.clone().unwrap();
        at(unify(subst, &placeholder, &ann_ty), span)?;
        // (2) subsumption: 注釈が推論された principal 型より一般的すぎないか。
        // principal を instantiate し、注釈を skolem 化して別 subst 上で unify する。
        // 失敗＝注釈が実際には持っていない一般性を主張している。
        let principal = generalize(env, subst, &placeholder);
        let inst = instantiate(&principal, vargen);
        let mut sk = HashMap::new();
        let ann_skolem = resolve_skolem(ann, &mut sk, vargen, arities)?;
        let mut scratch = subst.clone();
        if unify(&mut scratch, &inst, &ann_skolem).is_err() {
            return Err(LocatedError::at(
                AwaError::AnnotationTooGeneral {
                    inferred: render_type(&principal.ty),
                    annotation: render_type(&ann_ty),
                },
                span,
            ));
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
    methods: &MethodEnv,
) -> Result<TExpr, LocatedError> {
    let span = expr.span;
    let (kind, ty) = match &expr.kind {
        ExprKind::Int(n) => (TExprKind::Int(*n), Type::Int),
        ExprKind::Long(n) => (TExprKind::Long(*n), Type::Long),
        ExprKind::Float(n) => (TExprKind::Float(*n), Type::Float),
        ExprKind::Str(s) => (TExprKind::Str(s.clone()), Type::Str),
        ExprKind::Bool(b) => (TExprKind::Bool(*b), Type::Bool),
        // 変数は型環境を引き、スキームを instantiate して fresh な型に具体化する。
        // 多相な束縛（構築子 Some 等も含む）はここで使用箇所ごとに独立した型変数を得る。
        // 無ければ評価時と同じ UndefinedVariable。
        ExprKind::Var(name) => {
            let ty = env
                .get(name)
                .map(|scheme| instantiate(scheme, vargen))
                .ok_or_else(|| {
                    LocatedError::at(AwaError::UndefinedVariable(name.clone()), span)
                })?;
            (TExprKind::Var(name.clone()), ty)
        }
        // 単項マイナスは結果も同じ数値型（数値性は check_numeric で後から確認）。
        ExprKind::Neg(operand) => {
            let t = infer_expr(operand, env, subst, vargen, arities, methods)?;
            let ty = apply(subst, &t.ty);
            (TExprKind::Neg(Box::new(t)), ty)
        }
        ExprKind::BinOp(left, op, right) => {
            let lt = infer_expr(left, env, subst, vargen, arities, methods)?;
            let rt = infer_expr(right, env, subst, vargen, arities, methods)?;
            let ty = at(infer_binop(&lt.ty, op, &rt.ty, subst), span)?;
            (TExprKind::BinOp(Box::new(lt), op.clone(), Box::new(rt)), ty)
        }
        // 無名関数: 各引数に型を割り当て（注釈があればその型、無ければ fresh）、
        // その下で本体を推論する。戻り型注釈があれば本体型と unify する。
        // 注釈中の型変数はこのシグネチャ内で共有する（sig_vars を引数・戻り型で使い回す）。
        ExprKind::Lambda(params, ret_annotation, body) => {
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
            let body_t = infer_expr(body, &body_env, subst, vargen, arities, methods)?;
            if let Some(ann) = ret_annotation {
                let ann_ty = resolve_annotation(ann, &mut sig_vars, vargen, arities)?;
                at(unify(subst, &body_t.ty, &ann_ty), body_t.span)?;
            }
            let param_types = param_types.iter().map(|t| apply(subst, t)).collect();
            let ty = Type::Fun(param_types, Box::new(apply(subst, &body_t.ty)));
            (TExprKind::Lambda(param_names, Box::new(body_t)), ty)
        }
        // 呼び出し: callee を fn(実引数型...) -> 新しい戻り型 と unify する。
        // unify が「callee が関数か」「引数の個数と型が合うか」を一括で検査する。
        // 戻り型は fresh 型変数で置き、unify 後に確定したものを返す。
        ExprKind::Call(callee, args) => {
            // 文字列補間 `${e}` の脱糖。lexer は `${e}` を `__interp(e)` というトークン列に
            // 落とすので、ここで型を見て文字列化に置換する（型クラスなしの type-directed 変換）。
            // 置換後は通常の Call/If/Str ノードなので eval/codegen は __interp を一切見ない。
            if let ExprKind::Var(name) = &callee.kind {
                if name == "__interp" && args.len() == 1 {
                    let arg = infer_expr(&args[0], env, subst, vargen, arities, methods)?;
                    let arg_ty = apply(subst, &arg.ty);
                    // f(inner) の Call ノードを組み立てる小道具（f は inner.ty -> ret の組込）。
                    let conv = |fname: &str, inner: TExpr, ret: Type| TExpr {
                        ty: ret.clone(),
                        span,
                        kind: TExprKind::Call(
                            Box::new(TExpr {
                                ty: Type::Fun(vec![inner.ty.clone()], Box::new(ret)),
                                span: callee.span,
                                kind: TExprKind::Var(fname.to_string()),
                            }),
                            vec![inner],
                        ),
                    };
                    let lit = |s: &str| TExpr {
                        ty: Type::Str,
                        span,
                        kind: TExprKind::Str(s.to_string()),
                    };
                    let result = match arg_ty {
                        Type::Str => arg,
                        Type::Int => conv("int_to_string", arg, Type::Str),
                        Type::Float => conv("float_to_string", arg, Type::Str),
                        // Long は long_to_int で Int に落としてから文字列化。
                        Type::Long => conv(
                            "int_to_string",
                            conv("long_to_int", arg, Type::Int),
                            Type::Str,
                        ),
                        // Bool は専用組込を持たないので case で分岐（評価は 1 回）。
                        Type::Bool => TExpr {
                            ty: Type::Str,
                            span,
                            kind: TExprKind::Case(
                                Box::new(arg),
                                vec![
                                    (Pattern::Bool(true), lit("true")),
                                    (Pattern::Bool(false), lit("false")),
                                ],
                            ),
                        },
                        other => {
                            return Err(LocatedError::at(
                                AwaError::Unsupported(format!(
                                    "${{}} で文字列化できない型です: {other}（Int/Long/Float/Bool/String のみ）"
                                )),
                                args[0].span,
                            ));
                        }
                    };
                    return Ok(result);
                }
            }
            let callee_t = infer_expr(callee, env, subst, vargen, arities, methods)?;
            let mut targs = Vec::with_capacity(args.len());
            for a in args {
                targs.push(infer_expr(a, env, subst, vargen, arities, methods)?);
            }
            let ret = vargen.fresh();
            let arg_types = targs.iter().map(|t| t.ty.clone()).collect();
            let expected = Type::Fun(arg_types, Box::new(ret.clone()));
            at(unify(subst, &callee_t.ty, &expected), span)?;
            let ty = apply(subst, &ret);
            (TExprKind::Call(Box::new(callee_t), targs), ty)
        }
        // ブロックは新しいスコープ。親の型環境をクローンして始め、抜けると破棄する。
        ExprKind::Block(stmts) => {
            let mut block_env = env.clone();
            let (tstmts, ty) = infer_stmts(stmts, &mut block_env, subst, vargen, arities, methods)?;
            (TExprKind::Block(tstmts), ty)
        }
        // 配列リテラル [e1, ...]: 全要素を 1 つの要素型 elem に unify し、Array(elem) を返す。
        // 空 [] は elem が fresh のまま多相（Array(a)）になる。
        ExprKind::ArrayLit(elems) => {
            let elem = vargen.fresh();
            let mut telems = Vec::with_capacity(elems.len());
            for e in elems {
                let te = infer_expr(e, env, subst, vargen, arities, methods)?;
                at(unify(subst, &te.ty, &elem), e.span)?;
                telems.push(te);
            }
            let ty = Type::Con("Array".to_string(), vec![apply(subst, &elem)]);
            (TExprKind::ArrayLit(telems), ty)
        }
        // タプルリテラル (e1, ...): 各要素を独立に推論し Tuple(t1, ..., tn) を返す。
        ExprKind::TupleLit(elems) => {
            let mut telems = Vec::with_capacity(elems.len());
            let mut tys = Vec::with_capacity(elems.len());
            for e in elems {
                let te = infer_expr(e, env, subst, vargen, arities, methods)?;
                tys.push(apply(subst, &te.ty));
                telems.push(te);
            }
            (TExprKind::TupleLit(telems), Type::Con("Tuple".to_string(), tys))
        }
        // パターンマッチ: 主語の型に各腕のパターン型を unify、全腕の本体型を互いに unify。
        ExprKind::Case(subject, arms) => {
            let st = infer_expr(subject, env, subst, vargen, arities, methods)?;
            let result = vargen.fresh();
            let mut tarms = Vec::with_capacity(arms.len());
            for (pattern, body) in arms {
                // パターン由来のエラー（未定義構築子など）は腕の本体 span に寄せる。
                let (pty, bindings) = at(infer_pattern(pattern, env, subst, vargen), body.span)?;
                // パターンが主語と同じ型を覆うこと。
                at(unify(subst, &pty, &st.ty), body.span)?;
                // パターンが束縛した変数を単相スキームで環境に入れて本体を推論する。
                let mut arm_env = env.clone();
                for (name, t) in bindings {
                    arm_env.insert(name, Scheme::mono(t));
                }
                let bty = infer_expr(body, &arm_env, subst, vargen, arities, methods)?;
                at(unify(subst, &bty.ty, &result), bty.span)?;
                tarms.push((pattern.clone(), bty));
            }
            let ty = apply(subst, &result);
            (TExprKind::Case(Box::new(st), tarms), ty)
        }
        // メソッド呼び出し: レシーバの具体型でメソッドを静的に解決し、通常の関数適用
        // Call(Var("型頭#メソッド"), [recv, ...args]) へ lower する。
        ExprKind::MethodCall(receiver, method, args) => {
            let recv = infer_expr(receiver, env, subst, vargen, arities, methods)?;
            let recv_ty = apply(subst, &recv.ty);
            // レシーバの型頭が定まらなければ解決不能（型注釈を促す）。
            let head = type_head(&recv_ty)
                .ok_or_else(|| LocatedError::at(AwaError::MethodReceiverUnknown, span))?;
            let scheme = methods.get(&(head.clone(), method.clone())).ok_or_else(|| {
                LocatedError::at(
                    AwaError::UnknownMethod {
                        ty: head.clone(),
                        method: method.clone(),
                    },
                    span,
                )
            })?;
            let method_ty = instantiate(scheme, vargen);
            // 引数を推論。
            let mut targs = Vec::with_capacity(args.len());
            for a in args {
                targs.push(infer_expr(a, env, subst, vargen, arities, methods)?);
            }
            // メソッド型を fn(レシーバ型, 引数型...) -> 新戻り型 と unify。
            let ret = vargen.fresh();
            let mut call_param_tys = vec![recv.ty.clone()];
            call_param_tys.extend(targs.iter().map(|t| t.ty.clone()));
            let expected = Type::Fun(call_param_tys, Box::new(ret.clone()));
            at(unify(subst, &method_ty, &expected), span)?;
            let result_ty = apply(subst, &ret);
            // Call へ lower。callee はマングル名のグローバル関数 Var("型頭#メソッド")。
            let mangled = format!("{}#{}", head, method);
            let callee = TExpr {
                ty: apply(subst, &method_ty),
                span,
                kind: TExprKind::Var(mangled),
            };
            let mut call_args = vec![recv];
            call_args.extend(targs);
            (TExprKind::Call(Box::new(callee), call_args), result_ty)
        }
        // 修飾アクセス `x.member`（括弧なし）。レコードのフィールドアクセスなら projection へ
        // 脱糖する。そうでなければ未 import のモジュール参照＝エラー。
        ExprKind::Field(receiver, member) => {
            let rt = infer_expr(receiver, env, subst, vargen, arities, methods)?;
            let recv_ty = apply(subst, &rt.ty);
            if let Type::Con(tyname, _) = &recv_ty {
                if let Some(order) = record_order(tyname) {
                    let idx = order.iter().position(|f| f == member).ok_or_else(|| {
                        LocatedError::at(
                            AwaError::UnknownField { ty: tyname.clone(), field: member.clone() },
                            span,
                        )
                    })?;
                    // case receiver { Name(_, .., $field, .., _) -> $field } へ脱糖して推論。
                    let binder = "$field".to_string();
                    let mut pats = vec![Pattern::Wildcard; order.len()];
                    pats[idx] = Pattern::Var(binder.clone());
                    let proj = Expr {
                        kind: ExprKind::Case(
                            Box::new((**receiver).clone()),
                            vec![(
                                Pattern::Ctor(tyname.clone(), pats),
                                Expr { kind: ExprKind::Var(binder), span },
                            )],
                        ),
                        span,
                    };
                    return infer_expr(&proj, env, subst, vargen, arities, methods);
                }
            }
            let qual = match &receiver.kind {
                ExprKind::Var(q) => q.clone(),
                _ => member.clone(),
            };
            return Err(LocatedError::at(AwaError::UnknownModule(qual), span));
        }
        // レコードリテラルは check_program の脱糖パスで Call へ変換済み。
        ExprKind::RecordLit(..) => unreachable!("RecordLit は脱糖済み"),
        // レコード更新 { base | f = e }: base の型からフィールド順を得て case 再構築へ脱糖。
        ExprKind::RecordUpdate(base, inits) => {
            let bt = infer_expr(base, env, subst, vargen, arities, methods)?;
            let base_ty = apply(subst, &bt.ty);
            let tyname = match &base_ty {
                Type::Con(n, _) => n.clone(),
                other => {
                    return Err(LocatedError::at(AwaError::NotARecord(other.to_string()), span));
                }
            };
            let order = record_order(&tyname)
                .ok_or_else(|| LocatedError::at(AwaError::NotARecord(tyname.clone()), span))?;
            let mut overrides: HashMap<String, Expr> = HashMap::new();
            for (f, ie) in inits {
                if !order.contains(f) {
                    return Err(LocatedError::at(
                        AwaError::UnknownField { ty: tyname.clone(), field: f.clone() },
                        span,
                    ));
                }
                if overrides.insert(f.clone(), ie.clone()).is_some() {
                    return Err(LocatedError::at(AwaError::DuplicateField(f.clone()), span));
                }
            }
            let binders: Vec<String> = (0..order.len()).map(|i| format!("$u{}", i)).collect();
            let pat = Pattern::Ctor(
                tyname.clone(),
                binders.iter().map(|b| Pattern::Var(b.clone())).collect(),
            );
            let args: Vec<Expr> = order
                .iter()
                .enumerate()
                .map(|(i, f)| match overrides.get(f) {
                    Some(e) => e.clone(),
                    None => Expr { kind: ExprKind::Var(binders[i].clone()), span },
                })
                .collect();
            let rebuild = Expr {
                kind: ExprKind::Call(
                    Box::new(Expr { kind: ExprKind::Var(tyname.clone()), span }),
                    args,
                ),
                span,
            };
            let case = Expr {
                kind: ExprKind::Case(Box::new((**base).clone()), vec![(pat, rebuild)]),
                span,
            };
            return infer_expr(&case, env, subst, vargen, arities, methods);
        }
    };
    Ok(TExpr {
        ty,
        span: expr.span,
        kind,
    })
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
        Pattern::Int(_) => Ok((Type::Int, Vec::new())),
        Pattern::Str(_) => Ok((Type::Str, Vec::new())),
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
        // 配列パターン [a, b, ...]: 主語は Array(elem)。各要素パターンを elem に unify する。
        Pattern::Array(subs) => {
            let elem = vargen.fresh();
            let mut bindings = Vec::new();
            for sub in subs {
                let (sty, sbind) = infer_pattern(sub, env, subst, vargen)?;
                unify(subst, &sty, &elem)?;
                bindings.extend(sbind);
            }
            let ty = Type::Con("Array".to_string(), vec![apply(subst, &elem)]);
            Ok((ty, bindings))
        }
        // タプルパターン (a, b, ...): 各位置を独立に推論し Tuple(t1, ..., tn) を覆う。
        Pattern::Tuple(subs) => {
            let mut bindings = Vec::new();
            let mut tys = Vec::with_capacity(subs.len());
            for sub in subs {
                let (sty, sbind) = infer_pattern(sub, env, subst, vargen)?;
                tys.push(sty);
                bindings.extend(sbind);
            }
            Ok((Type::Con("Tuple".to_string(), tys), bindings))
        }
        // レコードパターンは check_program の脱糖パスで Ctor へ変換済み。
        Pattern::Record(..) => unreachable!("レコードパターンは脱糖済み"),
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
    methods: &MethodEnv,
) -> Result<(Vec<TStmt>, Type), LocatedError> {
    let mut local_arities = arities.clone();
    let mut local_methods = methods.clone();
    let mut tstmts = Vec::with_capacity(stmts.len());
    let mut last = Type::Unit;
    // ブロック内の相互再帰: 関数 let を本体推論の前に fresh プレースホルダで pre-declare する
    // （トップレベルと同じ letrec 化。定義順に関係なく互いを参照できる）。
    let mut prebound: HashMap<String, Type> = HashMap::new();
    for stmt in stmts {
        if let Stmt::Let(name, _, expr) = stmt {
            if matches!(expr.kind, ExprKind::Lambda(..)) {
                let ph = vargen.fresh();
                env.insert(name.clone(), Scheme::mono(ph.clone()));
                prebound.insert(name.clone(), ph);
            }
        }
    }
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, annotation, expr) => {
                let te = infer_let(
                    name,
                    annotation,
                    expr,
                    env,
                    subst,
                    vargen,
                    &local_arities,
                    &local_methods,
                    prebound.get(name).cloned(),
                )?;
                tstmts.push(TStmt::Let(name.clone(), te));
                last = Type::Unit; // let 文はブロック値に寄与しない
            }
            Stmt::Type(def) => {
                register_type(def, env, subst, vargen, &mut local_arities)?;
                tstmts.push(TStmt::Type(def.clone()));
                last = Type::Unit; // 型宣言もブロック値に寄与しない
            }
            Stmt::Impl(def) => {
                let tmethods =
                    register_impl(def, env, subst, vargen, &local_arities, &mut local_methods)?;
                tstmts.push(TStmt::Impl(tmethods));
                last = Type::Unit; // impl もブロック値に寄与しない
            }
            Stmt::Expr(expr) => {
                let te = infer_expr(expr, env, subst, vargen, &local_arities, &local_methods)?;
                last = te.ty.clone();
                tstmts.push(TStmt::Expr(te));
            }
        };
    }
    Ok((tstmts, apply(subst, &last)))
}

// 二項演算の型付け規則。「数値か」の判定は即時に行わず check_numeric（推論後の走査）に
// 委ねる＝do の束縛変数など「後で型が決まる」場合の誤判定を避ける（遅延チェック）。
//   算術 (+ - * /)   : 両辺 unify（同型必須）→ その型（数値性は後段で確認）
//   比較 (< > <= >=) : 両辺 unify → 結果 Bool（数値性は後段で確認）
//   連結 (<>)        : 両辺 String → String
//   等価 (== !=)     : 両辺 unify（任意の同型）→ Bool（構造比較）
// eval_binop の実行時規則と対応している。
fn infer_binop(lt: &Type, op: &Token, rt: &Type, subst: &mut Subst) -> Result<Type, AwaError> {
    match op {
        Token::Plus | Token::Minus | Token::Star | Token::Slash => {
            unify(subst, lt, rt)?; // 同型必須（1 + 1.0 はここで CannotUnify）
            Ok(apply(subst, lt))
        }
        Token::Lt | Token::Gt | Token::Le | Token::Ge => {
            unify(subst, lt, rt)?;
            Ok(Type::Bool)
        }
        Token::Concat => {
            unify(subst, lt, &Type::Str)?;
            unify(subst, rt, &Type::Str)?;
            Ok(Type::Str)
        }
        Token::EqEq | Token::NotEq => {
            unify(subst, lt, rt)?;
            Ok(Type::Bool)
        }
        // 論理積/和: 両辺 Bool、結果 Bool。
        Token::AndAnd | Token::OrOr => {
            unify(subst, lt, &Type::Bool)?;
            unify(subst, rt, &Type::Bool)?;
            Ok(Type::Bool)
        }
        // パーサが BinOp に入れるのは上記の演算子トークンのみ。
        other => unreachable!("二項演算子でないトークン: {:?}", other),
    }
}

// 数値型（Int/Long/Float）か判定。後段の check_numeric が解決済みの型に対して使う。
// 未確定（型変数）なら AmbiguousNumeric、数値でない具体型なら NotNumeric。
fn require_numeric(ty: &Type) -> Result<(), AwaError> {
    match ty {
        Type::Int | Type::Long | Type::Float => Ok(()),
        Type::Var(_) => Err(AwaError::AmbiguousNumeric),
        other => Err(AwaError::NotNumeric(other.to_string())),
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
            .map(|typed| {
                typed
                    .last()
                    .map(|ts| match ts {
                        TStmt::Let(_, te) | TStmt::Expr(te) => te.ty.clone(),
                        TStmt::Type(_) | TStmt::Impl(_) => Type::Unit,
                    })
                    .unwrap_or(Type::Unit)
            })
            .map_err(|e| e.error) // テストは AwaError（種類）と比較する
    }

    #[test]
    fn 算術はnum() {
        assert_eq!(infer("1 + 2 * 3"), Ok(Type::Int));
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
        assert_eq!(te.ty, Type::Fun(vec![Type::Int], Box::new(Type::Int)));
        let TExprKind::Lambda(params, body) = &te.kind else {
            panic!("lambda のはず");
        };
        assert_eq!(params, &vec!["x".to_string()]);
        // 本体はブロック { x + 1 }。ブロックの値型は Num。
        assert_eq!(body.ty, Type::Int);
        let TExprKind::Block(stmts) = &body.kind else {
            panic!("block のはず");
        };
        let TStmt::Expr(binop) = &stmts[0] else {
            panic!("式文のはず");
        };
        // x + 1 も、その左辺 x も Num に解決されている。
        assert_eq!(binop.ty, Type::Int);
        let TExprKind::BinOp(left, _, _) = &binop.kind else {
            panic!("binop のはず");
        };
        assert_eq!(left.ty, Type::Int);
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
    fn case両腕は同じ型() {
        assert_eq!(infer("case true { true -> 1; false -> 2 }"), Ok(Type::Int));
        assert!(infer("case true { true -> 1; false -> false }").is_err());
    }

    #[test]
    fn 関数の型を推論する() {
        // 引数の数値型は注釈で固定（注釈なしの a + b は AmbiguousNumeric）。
        assert_eq!(
            infer("fn(a: Int, b: Int) { a + b }"),
            Ok(Type::Fun(vec![Type::Int, Type::Int], Box::new(Type::Int)))
        );
    }

    #[test]
    fn 関数呼び出しの結果型() {
        assert_eq!(
            infer("let add = fn(a: Int, b: Int) { a + b }; add(1, 2)"),
            Ok(Type::Int)
        );
    }

    #[test]
    fn 注釈なしの算術は曖昧でエラー() {
        assert_eq!(infer("fn(a, b) { a + b }"), Err(AwaError::AmbiguousNumeric));
    }

    #[test]
    fn 関数でない値の呼び出しは型エラー() {
        assert!(infer("let x = 5; x(1)").is_err());
    }

    #[test]
    fn 再帰関数の型推論() {
        assert_eq!(
            infer("let fact = fn(n) { case n == 0 { true -> 1; false -> n * fact(n - 1) } }; fact(5)"),
            Ok(Type::Int)
        );
    }

    #[test]
    fn 高階関数の型推論() {
        assert_eq!(
            infer("let inc = fn(x) { x + 1 }; let twice = fn(f, x) { f(f(x)) }; twice(inc, 0)"),
            Ok(Type::Int)
        );
    }

    #[test]
    fn ブロックの型は末尾の式() {
        assert_eq!(infer("{ let a = 2; a * 3 }"), Ok(Type::Int));
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
            Ok(Type::Int)
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
            Ok(con("Opt", vec![Type::Int]))
        );
    }

    #[test]
    fn caseの結果は腕の本体型() {
        assert_eq!(
            infer("type Opt(a) { Some(a); None }; case Some(1) { Some(n) -> n + 1; None -> 0 }"),
            Ok(Type::Int)
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
            Ok(Type::Int)
        );
    }

    #[test]
    fn 入れ子パターンを推論する() {
        assert_eq!(
            infer(
                "type List(a) { Cons(a, List(a)); Nil }; \
                 case Cons(1, Nil) { Cons(h, _) -> h; Nil -> 0 }"
            ),
            Ok(Type::Int)
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
