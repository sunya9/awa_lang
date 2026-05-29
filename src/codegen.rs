use std::collections::{HashMap, HashSet};

use crate::ast::{Pattern, TExpr, TExprKind, TStmt, TypeExpr};
use crate::error::AwaError;
use crate::token::Token;
use crate::types::Type;

// ===== LLVM コード生成（バックエンド・マイルストーン2: クロージャ）=====
// 型付き AST → LLVM IR テキスト(.ll)。clang file.ll -o exe でネイティブ化。
//
// 値表現: Int/Long=i64、Float=double、Bool=i1、関数=クロージャ構造体 { ptr fn, ptr env }。
// クロージャ変換: すべての関数は第一引数に env ポインタを取り、本体冒頭でキャプチャ変数を
// env から load する。トップレベル関数はキャプチャなし（env=null・直接呼び出し）。ネスト lambda は
// 外側スコープの変数を malloc した env に詰めて捕獲する（解放しない＝v1 はリーク許容）。
// 未対応（ADT/String/case 等）は AwaError::Unsupported を返す（誤コンパイルしない）。

type Result<T> = std::result::Result<T, AwaError>;
const CLOSURE_TY: &str = "{ ptr, ptr }";

fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(AwaError::Unsupported(what.into()))
}

// awa の型 → LLVM 型。関数型はクロージャ構造体、ADT はヒープブロックへの ptr。
fn llvm_ty(ty: &Type) -> Result<String> {
    match ty {
        Type::Int | Type::Long => Ok("i64".into()),
        Type::Float => Ok("double".into()),
        Type::Bool => Ok("i1".into()),
        Type::Str => Ok("ptr".into()),
        Type::Fun(..) => Ok(CLOSURE_TY.into()),
        Type::Con(..) => Ok("ptr".into()),
        other => unsupported(format!("型 {}", other)),
    }
}

// 数値変換などの組込関数（codegen 側）。
fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "int_to_float"
            | "float_to_int"
            | "int_to_long"
            | "long_to_int"
            | "int_to_string"
            | "float_to_string"
    )
}

// Rust 文字列 → LLVM の文字列定数 `[N x i8] c"...\00"` と長さ N（null 込み）。
fn llvm_string_constant(s: &str) -> (String, usize) {
    let mut body = String::new();
    for &b in s.as_bytes() {
        if b == b'"' || b == b'\\' || !(0x20..0x7f).contains(&b) {
            body.push_str(&format!("\\{:02X}", b));
        } else {
            body.push(b as char);
        }
    }
    body.push_str("\\00");
    let n = s.len() + 1;
    (format!("[{} x i8] c\"{}\"", n, body), n)
}

// 構築子の情報（タグ・arity・所属型・フィールド型式・型パラメータ）。
#[derive(Clone)]
struct CtorInfo {
    tag: u32,
    arity: usize,
    type_params: Vec<String>,
    field_texprs: Vec<TypeExpr>,
}

// 型式＋param→具体型 の対応で、ADT フィールドの具体型を求める（単相前提）。
fn resolve_field(texpr: &TypeExpr, param_map: &HashMap<String, Type>) -> Result<Type> {
    match texpr {
        TypeExpr::Var(p) => param_map
            .get(p)
            .cloned()
            .ok_or_else(|| AwaError::Unsupported(format!("型パラメータ {}", p))),
        TypeExpr::Con(name, args) => {
            let resolved: Vec<Type> = args
                .iter()
                .map(|a| resolve_field(a, param_map))
                .collect::<Result<_>>()?;
            Ok(match name.as_str() {
                "Int" => Type::Int,
                "Long" => Type::Long,
                "Float" => Type::Float,
                "String" => Type::Str,
                "Bool" => Type::Bool,
                "Nil" => Type::Unit,
                _ => Type::Con(name.clone(), resolved),
            })
        }
        TypeExpr::Fun(ps, r) => {
            let ps: Vec<Type> = ps
                .iter()
                .map(|p| resolve_field(p, param_map))
                .collect::<Result<_>>()?;
            Ok(Type::Fun(ps, Box::new(resolve_field(r, param_map)?)))
        }
    }
}

// 構築子ブロックの LLVM 型: { i32 tag, field0, ... }。
fn block_ty(field_tys: &[Type]) -> Result<String> {
    let mut parts = vec!["i32".to_string()];
    for t in field_tys {
        parts.push(llvm_ty(t)?);
    }
    Ok(format!("{{ {} }}", parts.join(", ")))
}

fn is_float(ty: &Type) -> bool {
    matches!(ty, Type::Float)
}

// env 構造体の LLVM 型文字列（キャプチャ型の並び）。
fn env_struct_ty(caps: &[(String, Type)]) -> Result<String> {
    let tys: Vec<String> = caps.iter().map(|(_, t)| llvm_ty(t)).collect::<Result<_>>()?;
    Ok(format!("{{ {} }}", tys.join(", ")))
}

// ===== モジュール全体の状態 =====
struct Module {
    funcs: String,                          // 全関数定義
    fn_ctr: usize,                          // 匿名関数 @fnN の採番
    globals: HashMap<String, GlobalFn>,     // トップレベル関数名 → 定義（本体つき）
    instances: HashSet<String>,             // 発行済みの特殊化インスタンス（マングル名）
    ctors: HashMap<String, CtorInfo>,       // 構築子名 → 情報
    strings: String,                        // 文字列リテラルのグローバル定義
    str_ctr: usize,
}

// トップレベル関数の定義（単相化で本体を特殊化するため本体も持つ）。
#[derive(Clone)]
struct GlobalFn {
    param_names: Vec<String>,
    param_tys: Vec<Type>,
    ret: Type,
    body: TExpr,
}

impl GlobalFn {
    fn is_generic(&self) -> bool {
        self.param_tys.iter().any(has_var) || has_var(&self.ret)
    }
}

fn has_var(t: &Type) -> bool {
    match t {
        Type::Var(_) => true,
        Type::Fun(ps, r) => ps.iter().any(has_var) || has_var(r),
        Type::Con(_, args) => args.iter().any(has_var),
        _ => false,
    }
}

// 総称シグネチャ（型変数つき）を具体型に照合して置換を作る。
fn match_types(g: &Type, c: &Type, s: &mut crate::types::Subst) {
    match (g, c) {
        (Type::Var(id), _) => {
            s.insert(*id, c.clone());
        }
        (Type::Fun(gp, gr), Type::Fun(cp, cr)) => {
            for (a, b) in gp.iter().zip(cp.iter()) {
                match_types(a, b, s);
            }
            match_types(gr, cr, s);
        }
        (Type::Con(_, ga), Type::Con(_, ca)) => {
            for (a, b) in ga.iter().zip(ca.iter()) {
                match_types(a, b, s);
            }
        }
        _ => {}
    }
}

// 型を LLVM 記号に使える短い名前へ（特殊化のマングル用）。
fn mangle_ty(t: &Type) -> String {
    match t {
        Type::Int => "Int".into(),
        Type::Long => "Long".into(),
        Type::Float => "Float".into(),
        Type::Str => "String".into(),
        Type::Bool => "Bool".into(),
        Type::Unit => "Nil".into(),
        Type::Var(_) => "_".into(),
        Type::Fun(..) => "fn".into(),
        Type::Con(n, args) => {
            if args.is_empty() {
                n.clone()
            } else {
                format!("{}_{}", n, args.iter().map(mangle_ty).collect::<Vec<_>>().join("_"))
            }
        }
    }
}

fn mangle(name: &str, params: &[Type], ret: &Type) -> String {
    let mut parts: Vec<String> = params.iter().map(mangle_ty).collect();
    parts.push(mangle_ty(ret));
    format!("{}${}", name, parts.join("$"))
}

// 型付き AST の全ノードの型に置換を適用する（特殊化＝総称本体を具体化する）。
fn subst_types(te: &mut TExpr, s: &crate::types::Subst) {
    te.ty = crate::types::apply(s, &te.ty);
    match &mut te.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_)
        | TExprKind::Var(_) => {}
        TExprKind::Neg(x) => subst_types(x, s),
        TExprKind::BinOp(l, _, r) => {
            subst_types(l, s);
            subst_types(r, s);
        }
        TExprKind::If(c, t, e) => {
            subst_types(c, s);
            subst_types(t, s);
            subst_types(e, s);
        }
        TExprKind::Lambda(_, b) => subst_types(b, s),
        TExprKind::Call(c, args) => {
            subst_types(c, s);
            for a in args {
                subst_types(a, s);
            }
        }
        TExprKind::Block(stmts) => {
            for st in stmts {
                match st {
                    TStmt::Let(_, e) | TStmt::Expr(e) => subst_types(e, s),
                    TStmt::Type(_) | TStmt::Impl(_) => {}
                }
            }
        }
        TExprKind::Case(subj, arms) => {
            subst_types(subj, s);
            for (_, b) in arms {
                subst_types(b, s);
            }
        }
    }
}

// 呼び出し対象のトップレベル関数を具体型で特殊化し、呼ぶべき LLVM 名を返す。
// 単相関数は @name（pass2 で発行済み）。総称関数は @name$型... を遅延発行（メモ）。
fn ensure_instance(m: &mut Module, name: &str, params: &[Type], ret: &Type) -> Result<String> {
    let gf = m.globals.get(name).expect("known top-level function").clone();
    if !gf.is_generic() {
        return Ok(name.to_string());
    }
    let mangled = mangle(name, params, ret);
    if m.instances.contains(&mangled) {
        return Ok(mangled);
    }
    m.instances.insert(mangled.clone());
    // 総称シグネチャ → 具体型の置換を作り、本体に適用して特殊化。
    let mut s = crate::types::Subst::new();
    for (g, c) in gf.param_tys.iter().zip(params.iter()) {
        match_types(g, c, &mut s);
    }
    match_types(&gf.ret, ret, &mut s);
    let mut body = gf.body.clone();
    subst_types(&mut body, &s);
    let typed_params: Vec<(String, Type)> = gf
        .param_names
        .iter()
        .cloned()
        .zip(params.iter().cloned())
        .collect();
    gen_function(m, &mangled, &[], &typed_params, ret, &body)?;
    Ok(mangled)
}

impl Module {
    fn fresh_fn(&mut self) -> String {
        let n = self.fn_ctr;
        self.fn_ctr += 1;
        format!("fn{}", n)
    }
    // 文字列リテラルをグローバル定数として登録し、その ptr（@.strN）を返す。
    fn intern_string(&mut self, s: &str) -> String {
        let label = format!("@.str{}", self.str_ctr);
        self.str_ctr += 1;
        let (constant, _) = llvm_string_constant(s);
        self.strings
            .push_str(&format!("{} = private unnamed_addr constant {}\n", label, constant));
        label
    }
}

// ===== 1 関数本体の生成状態 =====
struct Gen {
    out: String,
    reg: usize,
    scope: HashMap<String, (String, Type)>, // 変数名 → (alloca ptr, 型)
}

impl Gen {
    fn new() -> Self {
        Gen {
            out: String::new(),
            reg: 0,
            scope: HashMap::new(),
        }
    }
    fn fresh(&mut self) -> usize {
        let n = self.reg;
        self.reg += 1;
        n
    }
    fn line(&mut self, s: &str) {
        self.out.push_str("  ");
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn label(&mut self, l: &str) {
        self.out.push_str(l);
        self.out.push_str(":\n");
    }
    fn bind_local(&mut self, name: &str, operand: &str, ty: &Type) -> Result<()> {
        let llty = llvm_ty(ty)?;
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, llty));
        self.line(&format!("store {} {}, ptr {}", llty, operand, slot));
        self.scope.insert(name.to_string(), (slot, ty.clone()));
        Ok(())
    }

    // 式 → LLVM オペランド。
    fn expr(&mut self, e: &TExpr, m: &mut Module) -> Result<String> {
        match &e.kind {
            TExprKind::Int(n) | TExprKind::Long(n) => Ok(n.to_string()),
            TExprKind::Float(n) => Ok(format!("0x{:016X}", n.to_bits())),
            TExprKind::Bool(b) => Ok(if *b { "1".into() } else { "0".into() }),
            // 文字列リテラル → グローバル定数への ptr。
            TExprKind::Str(s) => Ok(m.intern_string(s)),
            TExprKind::Var(name) => self.var(name, &e.ty, m),
            TExprKind::Neg(x) => {
                let v = self.expr(x, m)?;
                let r = format!("%t{}", self.fresh());
                if is_float(&x.ty) {
                    self.line(&format!("{} = fneg double {}", r, v));
                } else {
                    self.line(&format!("{} = sub i64 0, {}", r, v));
                }
                Ok(r)
            }
            TExprKind::BinOp(l, op, r) => self.binop(l, op, r, m),
            TExprKind::If(c, t, e2) => self.if_expr(c, t, e2, m),
            TExprKind::Call(callee, args) => self.call(callee, args, m),
            TExprKind::Block(stmts) => self.block(stmts, m),
            TExprKind::Lambda(params, body) => self.lambda(params, body, &e.ty, m),
            TExprKind::Case(subject, arms) => self.compile_case(subject, arms, m),
        }
    }

    // case を逐次マッチへコンパイル。各腕でパターンを検査し、不一致なら次の腕へ、
    // 一致したらフィールドを束縛して本体を評価する。網羅性は静的検査済み（最後は unreachable）。
    fn compile_case(
        &mut self,
        subject: &TExpr,
        arms: &[(Pattern, TExpr)],
        m: &mut Module,
    ) -> Result<String> {
        let sv = self.expr(subject, m)?;
        let sty = subject.ty.clone();
        let res_ty = llvm_ty(&arms[0].1.ty)?;
        let res = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", res, res_ty));
        let end = format!("end{}", self.fresh());
        for (pat, body) in arms {
            let next = format!("arm{}", self.fresh());
            let saved = self.scope.clone();
            self.match_pattern_ir(pat, &sv, &sty, &next, m)?;
            let bv = self.expr(body, m)?;
            self.line(&format!("store {} {}, ptr {}", res_ty, bv, res));
            self.line(&format!("br label %{}", end));
            self.scope = saved;
            self.label(&next);
        }
        // どの腕にも当たらない経路（網羅性が保証するので到達しない）。
        self.line("unreachable");
        self.label(&end);
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = load {}, ptr {}", r, res_ty, res));
        Ok(r)
    }

    // パターンを値に照合する IR を出す。不一致なら fail へ br、一致なら束縛して継続。
    fn match_pattern_ir(
        &mut self,
        pat: &Pattern,
        value: &str,
        value_ty: &Type,
        fail: &str,
        m: &Module,
    ) -> Result<()> {
        match pat {
            Pattern::Wildcard => Ok(()),
            Pattern::Var(n) => self.bind_local(n, value, value_ty),
            Pattern::Bool(b) => {
                self.eq_or_fail("i1", value, if *b { "1" } else { "0" }, fail);
                Ok(())
            }
            Pattern::Int(n) => {
                let v = n.to_string();
                self.eq_or_fail("i64", value, &v, fail);
                Ok(())
            }
            Pattern::Str(_) => unsupported("文字列パターン（codegen 未対応）"),
            Pattern::Or(_) => unsupported("or パターン（codegen 未対応）"),
            Pattern::Ctor(name, subs) => {
                let ci = m
                    .ctors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| AwaError::Unsupported(format!("未知の構築子 {}", name)))?;
                // tag は先頭の i32。
                let tag = format!("%t{}", self.fresh());
                self.line(&format!("{} = load i32, ptr {}", tag, value));
                self.eq_or_fail("i32", &tag, &ci.tag.to_string(), fail);
                if subs.is_empty() {
                    return Ok(());
                }
                // フィールド型を解決（型パラメータ → scrutinee の具体型引数）。
                let Type::Con(_, typeargs) = value_ty else {
                    return unsupported("構築子パターンの主語が ADT でない");
                };
                let param_map: HashMap<String, Type> = ci
                    .type_params
                    .iter()
                    .cloned()
                    .zip(typeargs.iter().cloned())
                    .collect();
                let field_tys: Vec<Type> = ci
                    .field_texprs
                    .iter()
                    .map(|t| resolve_field(t, &param_map))
                    .collect::<Result<_>>()?;
                let bty = block_ty(&field_tys)?;
                for (i, (sub, fty)) in subs.iter().zip(field_tys.iter()).enumerate() {
                    let fp = format!("%t{}", self.fresh());
                    self.line(&format!(
                        "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                        fp,
                        bty,
                        value,
                        i + 1
                    ));
                    let fv = format!("%t{}", self.fresh());
                    self.line(&format!("{} = load {}, ptr {}", fv, llvm_ty(fty)?, fp));
                    self.match_pattern_ir(sub, &fv, fty, fail, m)?;
                }
                Ok(())
            }
        }
    }

    // value == lit なら継続、違えば fail へ。
    fn eq_or_fail(&mut self, llty: &str, value: &str, lit: &str, fail: &str) {
        let c = format!("%t{}", self.fresh());
        self.line(&format!("{} = icmp eq {} {}, {}", c, llty, value, lit));
        let cont = format!("cont{}", self.fresh());
        self.line(&format!("br i1 {}, label %{}, label %{}", c, cont, fail));
        self.label(&cont);
    }

    // 変数参照: ローカル/キャプチャ → load、ヌル構築子 → 構築、トップレベル関数 → クロージャ値。
    fn var(&mut self, name: &str, ty: &Type, m: &mut Module) -> Result<String> {
        if let Some((slot, vty)) = self.scope.get(name).cloned() {
            let llty = llvm_ty(&vty)?;
            let r = format!("%t{}", self.fresh());
            self.line(&format!("{} = load {}, ptr {}", r, llty, slot));
            Ok(r)
        } else if let Some(ci) = m.ctors.get(name).cloned() {
            if ci.arity == 0 {
                self.construct(&ci, &[]) // ヌル構築子
            } else {
                unsupported(format!("構築子を値として使う: {}", name))
            }
        } else if m.globals.contains_key(name) {
            // トップレベル関数を値として使う → 具体型で特殊化し、クロージャ {@mangled, null}。
            let Type::Fun(params, ret) = ty else {
                return unsupported("関数値の型が解決できない");
            };
            let mangled = ensure_instance(m, name, params, ret)?;
            let c0 = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = insertvalue {} undef, ptr @{}, 0",
                c0, CLOSURE_TY, mangled
            ));
            let c1 = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = insertvalue {} {}, ptr null, 1",
                c1, CLOSURE_TY, c0
            ));
            Ok(c1)
        } else {
            unsupported(format!("捕獲できない変数: {}（トップレベル値の参照は未対応）", name))
        }
    }

    fn binop(&mut self, l: &TExpr, op: &Token, r: &TExpr, m: &mut Module) -> Result<String> {
        let lv = self.expr(l, m)?;
        let rv = self.expr(r, m)?;
        // 文字列の連結と等価はランタイム関数（ptr 比較ではなく中身で比べる）。
        if matches!(l.ty, Type::Str) {
            return self.str_binop(op, &lv, &rv);
        }
        let llty = llvm_ty(&l.ty)?;
        let float = is_float(&l.ty);
        let reg = format!("%t{}", self.fresh());
        let arith = match op {
            Token::Plus => Some(if float { "fadd" } else { "add" }),
            Token::Minus => Some(if float { "fsub" } else { "sub" }),
            Token::Star => Some(if float { "fmul" } else { "mul" }),
            Token::Slash => Some(if float { "fdiv" } else { "sdiv" }),
            _ => None,
        };
        if let Some(instr) = arith {
            self.line(&format!("{} = {} {} {}, {}", reg, instr, llty, lv, rv));
            return Ok(reg);
        }
        let pred = match (op, float) {
            (Token::Lt, false) => "icmp slt",
            (Token::Gt, false) => "icmp sgt",
            (Token::Le, false) => "icmp sle",
            (Token::Ge, false) => "icmp sge",
            (Token::EqEq, false) => "icmp eq",
            (Token::NotEq, false) => "icmp ne",
            (Token::Lt, true) => "fcmp olt",
            (Token::Gt, true) => "fcmp ogt",
            (Token::Le, true) => "fcmp ole",
            (Token::Ge, true) => "fcmp oge",
            (Token::EqEq, true) => "fcmp oeq",
            (Token::NotEq, true) => "fcmp one",
            (Token::Concat, _) => return unsupported("文字列連結 <>"),
            (other, _) => unreachable!("二項演算子でないトークン: {:?}", other),
        };
        self.line(&format!("{} = {} {} {}, {}", reg, pred, llty, lv, rv));
        Ok(reg)
    }

    // 文字列の連結 <> と等価 ==/!=（中身比較）。
    fn str_binop(&mut self, op: &Token, lv: &str, rv: &str) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match op {
            Token::Concat => {
                self.line(&format!("{} = call ptr @awa_concat(ptr {}, ptr {})", r, lv, rv));
                Ok(r)
            }
            Token::EqEq | Token::NotEq => {
                let c = format!("%t{}", self.fresh());
                self.line(&format!("{} = call i32 @strcmp(ptr {}, ptr {})", c, lv, rv));
                let pred = if matches!(op, Token::EqEq) { "eq" } else { "ne" };
                self.line(&format!("{} = icmp {} i32 {}, 0", r, pred, c));
                Ok(r)
            }
            _ => unsupported("文字列にこの演算子は使えません"),
        }
    }

    // 組込変換関数（すべて単項）。
    fn builtin_call(&mut self, name: &str, arg: &str, arg_ty: &Type) -> Result<String> {
        let r = format!("%t{}", self.fresh());
        match name {
            "int_to_float" => {
                self.line(&format!("{} = sitofp i64 {} to double", r, arg));
                Ok(r)
            }
            "float_to_int" => {
                self.line(&format!("{} = fptosi double {} to i64", r, arg));
                Ok(r)
            }
            // i64 ↔ i64（nominal 区別のみ）。実体は同じ。
            "int_to_long" | "long_to_int" => Ok(arg.to_string()),
            "int_to_string" | "float_to_string" => {
                let (fmt, llty) = if matches!(arg_ty, Type::Float) {
                    ("@.ff", "double")
                } else {
                    ("@.fi", "i64")
                };
                let buf = format!("%t{}", self.fresh());
                self.line(&format!("{} = call ptr @malloc(i64 32)", buf));
                let n = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = call i32 (ptr, i64, ptr, ...) @snprintf(ptr {}, i64 32, ptr {}, {} {})",
                    n, buf, fmt, llty, arg
                ));
                Ok(buf)
            }
            _ => unsupported(format!("組込関数 {}", name)),
        }
    }

    fn if_expr(&mut self, c: &TExpr, t: &TExpr, e: &TExpr, m: &mut Module) -> Result<String> {
        let res_ty = llvm_ty(&t.ty)?;
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, res_ty));
        let cond = self.expr(c, m)?;
        let id = self.fresh();
        let (tl, el, ml) = (format!("then{id}"), format!("else{id}"), format!("merge{id}"));
        self.line(&format!("br i1 {}, label %{}, label %{}", cond, tl, el));
        self.label(&tl);
        let tv = self.expr(t, m)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, tv, slot));
        self.line(&format!("br label %{}", ml));
        self.label(&el);
        let ev = self.expr(e, m)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, ev, slot));
        self.line(&format!("br label %{}", ml));
        self.label(&ml);
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = load {}, ptr {}", r, res_ty, slot));
        Ok(r)
    }

    // 構築子ブロックを malloc して tag と各フィールドを格納し、その ptr を返す。
    fn construct(&mut self, ci: &CtorInfo, fields: &[(String, Type)]) -> Result<String> {
        let field_tys: Vec<Type> = fields.iter().map(|(_, t)| t.clone()).collect();
        let bty = block_ty(&field_tys)?;
        let szp = format!("%t{}", self.fresh());
        self.line(&format!("{} = getelementptr {}, ptr null, i32 1", szp, bty));
        let sz = format!("%t{}", self.fresh());
        self.line(&format!("{} = ptrtoint ptr {} to i64", sz, szp));
        let p = format!("%t{}", self.fresh());
        self.line(&format!("{} = call ptr @malloc(i64 {})", p, sz));
        let tp = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = getelementptr {}, ptr {}, i32 0, i32 0",
            tp, bty, p
        ));
        self.line(&format!("store i32 {}, ptr {}", ci.tag, tp));
        for (i, (op, ty)) in fields.iter().enumerate() {
            let fp = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                fp,
                bty,
                p,
                i + 1
            ));
            self.line(&format!("store {} {}, ptr {}", llvm_ty(ty)?, op, fp));
        }
        Ok(p)
    }

    fn call(&mut self, callee: &TExpr, args: &[TExpr], m: &mut Module) -> Result<String> {
        // 引数の型と値。
        let mut arg_parts = Vec::with_capacity(args.len());
        // callee が組込変換関数なら専用 IR（すべて単項）。
        if let TExprKind::Var(name) = &callee.kind {
            if is_builtin(name) {
                let v = self.expr(&args[0], m)?;
                return self.builtin_call(name, &v, &args[0].ty);
            }
        }
        // callee が構築子なら、引数を評価して Data ブロックを構築。
        if let TExprKind::Var(name) = &callee.kind {
            if let Some(ci) = m.ctors.get(name).cloned() {
                let mut fields = Vec::with_capacity(args.len());
                for a in args {
                    let v = self.expr(a, m)?;
                    fields.push((v, a.ty.clone()));
                }
                return self.construct(&ci, &fields);
            }
        }
        // callee がトップレベル関数名なら直接呼び出し（env=null）。具体型で特殊化（単相化）。
        if let TExprKind::Var(name) = &callee.kind {
            if m.globals.contains_key(name) {
                let Type::Fun(cparams, cret) = &callee.ty else {
                    return unsupported("関数呼び出しの型が解決できない");
                };
                let cparams = cparams.clone();
                let cret = (**cret).clone();
                let mangled = ensure_instance(m, name, &cparams, &cret)?;
                let ret_ll = llvm_ty(&cret)?;
                for (a, pty) in args.iter().zip(cparams.iter()) {
                    let v = self.expr(a, m)?;
                    arg_parts.push(format!("{} {}", llvm_ty(pty)?, v));
                }
                let r = format!("%t{}", self.fresh());
                let mut call_args = vec!["ptr null".to_string()];
                call_args.extend(arg_parts);
                self.line(&format!(
                    "{} = call {} @{}({})",
                    r,
                    ret_ll,
                    mangled,
                    call_args.join(", ")
                ));
                return Ok(r);
            }
        }
        // 間接呼び出し: callee をクロージャ値に評価し fn/env を取り出して呼ぶ。
        let Type::Fun(params, ret) = &callee.ty else {
            return unsupported("関数でない値の呼び出し");
        };
        let ret_ll = llvm_ty(ret)?;
        let param_lls: Vec<String> = params.iter().map(llvm_ty).collect::<Result<_>>()?;
        let clos = self.expr(callee, m)?;
        let fnp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 0", fnp, CLOSURE_TY, clos));
        let envp = format!("%t{}", self.fresh());
        self.line(&format!("{} = extractvalue {} {}, 1", envp, CLOSURE_TY, clos));
        for (a, pll) in args.iter().zip(param_lls.iter()) {
            let v = self.expr(a, m)?;
            arg_parts.push(format!("{} {}", pll, v));
        }
        let r = format!("%t{}", self.fresh());
        let mut call_args = vec![format!("ptr {}", envp)];
        call_args.extend(arg_parts);
        self.line(&format!(
            "{} = call {} {}({})",
            r,
            ret_ll,
            fnp,
            call_args.join(", ")
        ));
        Ok(r)
    }

    fn block(&mut self, stmts: &[TStmt], m: &mut Module) -> Result<String> {
        let saved = self.scope.clone();
        let mut last = String::new();
        for s in stmts {
            match s {
                TStmt::Let(name, rhs) => {
                    let v = self.expr(rhs, m)?;
                    self.bind_local(name, &v, &rhs.ty)?;
                    last = String::new();
                }
                TStmt::Expr(e) => last = self.expr(e, m)?,
                TStmt::Type(_) | TStmt::Impl(_) => return unsupported("ブロック内の type/impl"),
            }
        }
        self.scope = saved;
        if last.is_empty() {
            unsupported("値を持たないブロック")
        } else {
            Ok(last)
        }
    }

    // ネスト lambda: キャプチャを env(malloc)に詰めてクロージャ値 {@fnN, env} を作る。
    fn lambda(
        &mut self,
        params: &[String],
        body: &TExpr,
        fn_ty: &Type,
        m: &mut Module,
    ) -> Result<String> {
        let Type::Fun(param_tys, ret) = fn_ty else {
            return unsupported("関数の型が解決できない");
        };
        // キャプチャ＝本体の自由変数のうち、トップレベル関数でないもの。型は現スコープから。
        let fv = free_vars(params, body);
        let mut caps: Vec<(String, Type)> = Vec::new();
        for name in &fv {
            if m.globals.contains_key(name) {
                continue; // トップレベル関数は名前参照（捕獲しない）
            }
            match self.scope.get(name) {
                Some((_, ty)) => caps.push((name.clone(), ty.clone())),
                None => return unsupported(format!("捕獲できない変数: {}", name)),
            }
        }
        let fn_name = m.fresh_fn();
        let typed_params: Vec<(String, Type)> = params
            .iter()
            .cloned()
            .zip(param_tys.iter().cloned())
            .collect();
        gen_function(m, &fn_name, &caps, &typed_params, ret, body)?;

        // env を malloc してキャプチャを格納。
        let env_operand = if caps.is_empty() {
            "null".to_string()
        } else {
            let env_ty = env_struct_ty(&caps)?;
            // sizeof(env) を getelementptr で求める。
            let szp = format!("%t{}", self.fresh());
            self.line(&format!(
                "{} = getelementptr {}, ptr null, i32 1",
                szp, env_ty
            ));
            let sz = format!("%t{}", self.fresh());
            self.line(&format!("{} = ptrtoint ptr {} to i64", sz, szp));
            let env = format!("%t{}", self.fresh());
            self.line(&format!("{} = call ptr @malloc(i64 {})", env, sz));
            // 各キャプチャを格納（現スコープから load → env に store）。
            for (i, (cname, cty)) in caps.iter().enumerate() {
                let cv = self.var(cname, cty, m)?;
                let fp = format!("%t{}", self.fresh());
                self.line(&format!(
                    "{} = getelementptr {}, ptr {}, i32 0, i32 {}",
                    fp, env_ty, env, i
                ));
                self.line(&format!("store {} {}, ptr {}", llvm_ty(cty)?, cv, fp));
            }
            env
        };
        // クロージャ {@fnN, env} を組む。
        let c0 = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = insertvalue {} undef, ptr @{}, 0",
            c0, CLOSURE_TY, fn_name
        ));
        let c1 = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = insertvalue {} {}, ptr {}, 1",
            c1, CLOSURE_TY, c0, env_operand
        ));
        Ok(c1)
    }
}

// 1 つの関数定義を生成して module.funcs に追記する。
// シグネチャ: define <ret> @name(ptr %env, <params...>)。本体冒頭でキャプチャを env から load。
fn gen_function(
    m: &mut Module,
    name: &str,
    caps: &[(String, Type)],
    params: &[(String, Type)],
    ret: &Type,
    body: &TExpr,
) -> Result<()> {
    let ret_ll = llvm_ty(ret)?;
    let mut g = Gen::new();
    // 引数ヘッダ（先頭は env）。
    let mut header = vec!["ptr %env".to_string()];
    for (i, (_, pty)) in params.iter().enumerate() {
        header.push(format!("{} %arg{}", llvm_ty(pty)?, i));
    }
    // キャプチャを env から load してローカルに束縛。
    if !caps.is_empty() {
        let env_ty = env_struct_ty(caps)?;
        for (i, (cname, cty)) in caps.iter().enumerate() {
            let fp = format!("%t{}", g.fresh());
            g.line(&format!(
                "{} = getelementptr {}, ptr %env, i32 0, i32 {}",
                fp, env_ty, i
            ));
            let v = format!("%t{}", g.fresh());
            g.line(&format!("{} = load {}, ptr {}", v, llvm_ty(cty)?, fp));
            g.bind_local(cname, &v, cty)?;
        }
    }
    // 引数を束縛。
    for (i, (pname, pty)) in params.iter().enumerate() {
        g.bind_local(pname, &format!("%arg{}", i), pty)?;
    }
    let result = g.expr(body, m)?;
    g.line(&format!("ret {} {}", ret_ll, result));
    m.funcs.push_str(&format!(
        "define {} @{}({}) {{\n{}}}\n\n",
        ret_ll,
        name,
        header.join(", "),
        g.out
    ));
    Ok(())
}

// 本体の自由変数（params と内側で束縛される名前を除く）。決定的な順序で返す。
fn free_vars(params: &[String], body: &TExpr) -> Vec<String> {
    let mut bound: HashSet<String> = params.iter().cloned().collect();
    let mut free: Vec<String> = Vec::new();
    fv(body, &mut bound, &mut free);
    free.sort();
    free.dedup();
    free
}

fn fv(e: &TExpr, bound: &HashSet<String>, free: &mut Vec<String>) {
    match &e.kind {
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_) => {}
        TExprKind::Var(n) => {
            if !bound.contains(n) {
                free.push(n.clone());
            }
        }
        TExprKind::Neg(x) => fv(x, bound, free),
        TExprKind::BinOp(l, _, r) => {
            fv(l, bound, free);
            fv(r, bound, free);
        }
        TExprKind::If(c, t, e2) => {
            fv(c, bound, free);
            fv(t, bound, free);
            fv(e2, bound, free);
        }
        TExprKind::Lambda(ps, b) => {
            let mut b2 = bound.clone();
            for p in ps {
                b2.insert(p.clone());
            }
            fv(b, &b2, free);
        }
        TExprKind::Call(c, args) => {
            fv(c, bound, free);
            for a in args {
                fv(a, bound, free);
            }
        }
        TExprKind::Block(stmts) => {
            let mut b2 = bound.clone();
            for s in stmts {
                match s {
                    TStmt::Let(n, rhs) => {
                        fv(rhs, &b2, free);
                        b2.insert(n.clone());
                    }
                    TStmt::Expr(e2) => fv(e2, &b2, free),
                    TStmt::Type(_) | TStmt::Impl(_) => {}
                }
            }
        }
        TExprKind::Case(subj, arms) => {
            fv(subj, bound, free);
            for (pat, body) in arms {
                let mut b2 = bound.clone();
                pattern_vars(pat, &mut b2);
                fv(body, &b2, free);
            }
        }
    }
}

fn pattern_vars(p: &crate::ast::Pattern, out: &mut HashSet<String>) {
    use crate::ast::Pattern;
    match p {
        Pattern::Var(n) => {
            out.insert(n.clone());
        }
        Pattern::Ctor(_, subs) | Pattern::Or(subs) => {
            for s in subs {
                pattern_vars(s, out);
            }
        }
        _ => {}
    }
}

// プログラム全体 → LLVM IR テキスト。
pub fn compile_program(stmts: &[TStmt]) -> Result<String> {
    let mut globals: HashMap<String, GlobalFn> = HashMap::new();
    for s in stmts {
        if let TStmt::Let(name, te) = s {
            if let TExprKind::Lambda(param_names, body) = &te.kind {
                let Type::Fun(params, ret) = &te.ty else {
                    return unsupported("関数の型が解決できない");
                };
                globals.insert(
                    name.clone(),
                    GlobalFn {
                        param_names: param_names.clone(),
                        param_tys: params.clone(),
                        ret: (**ret).clone(),
                        body: (**body).clone(),
                    },
                );
            }
        }
    }

    // 構築子レジストリ（タグは型ごとに宣言順 0,1,...）。
    let mut ctors: HashMap<String, CtorInfo> = HashMap::new();
    for s in stmts {
        if let TStmt::Type(def) = s {
            for (tag, v) in def.variants.iter().enumerate() {
                ctors.insert(
                    v.name.clone(),
                    CtorInfo {
                        tag: tag as u32,
                        arity: v.fields.len(),
                        type_params: def.params.clone(),
                        field_texprs: v.fields.clone(),
                    },
                );
            }
        }
    }

    let mut m = Module {
        funcs: String::new(),
        fn_ctr: 0,
        globals,
        instances: HashSet::new(),
        ctors,
        strings: String::new(),
        str_ctr: 0,
    };

    // 単相なトップレベル関数だけを @name で先に出力する。
    // 総称関数は使用箇所の具体型ごとに ensure_instance が遅延発行する。
    let mono: Vec<String> = m
        .globals
        .iter()
        .filter(|(_, gf)| !gf.is_generic())
        .map(|(n, _)| n.clone())
        .collect();
    for name in mono {
        let gf = m.globals.get(&name).unwrap().clone();
        let typed_params: Vec<(String, Type)> = gf
            .param_names
            .iter()
            .cloned()
            .zip(gf.param_tys.iter().cloned())
            .collect();
        gen_function(&mut m, &name, &[], &typed_params, &gf.ret, &gf.body)?;
    }

    // main。
    let main = compile_main(stmts, &mut m)?;

    let mut module = String::new();
    // ランタイム宣言。
    module.push_str("declare i32 @printf(ptr, ...)\n");
    module.push_str("declare ptr @malloc(i64)\n");
    module.push_str("declare i64 @strlen(ptr)\n");
    module.push_str("declare ptr @memcpy(ptr, ptr, i64)\n");
    module.push_str("declare i32 @strcmp(ptr, ptr)\n");
    module.push_str("declare i32 @snprintf(ptr, i64, ptr, ...)\n");
    // 書式・定数文字列。
    module.push_str("@.di = private unnamed_addr constant [6 x i8] c\"%lld\\0A\\00\"\n");
    module.push_str("@.df = private unnamed_addr constant [4 x i8] c\"%g\\0A\\00\"\n");
    module.push_str("@.ds = private unnamed_addr constant [4 x i8] c\"%s\\0A\\00\"\n");
    module.push_str("@.fi = private unnamed_addr constant [5 x i8] c\"%lld\\00\"\n");
    module.push_str("@.ff = private unnamed_addr constant [3 x i8] c\"%g\\00\"\n");
    module.push_str("@.t = private unnamed_addr constant [5 x i8] c\"true\\00\"\n");
    module.push_str("@.f = private unnamed_addr constant [6 x i8] c\"false\\00\"\n");
    module.push_str(&m.strings);
    // 文字列連結ランタイム。
    module.push_str(
        "\ndefine ptr @awa_concat(ptr %a, ptr %b) {\n\
         \x20 %la = call i64 @strlen(ptr %a)\n\
         \x20 %lb = call i64 @strlen(ptr %b)\n\
         \x20 %lab = add i64 %la, %lb\n\
         \x20 %sz = add i64 %lab, 1\n\
         \x20 %buf = call ptr @malloc(i64 %sz)\n\
         \x20 %c1 = call ptr @memcpy(ptr %buf, ptr %a, i64 %la)\n\
         \x20 %tail = getelementptr i8, ptr %buf, i64 %la\n\
         \x20 %lb1 = add i64 %lb, 1\n\
         \x20 %c2 = call ptr @memcpy(ptr %tail, ptr %b, i64 %lb1)\n\
         \x20 ret ptr %buf\n\
         }\n\n",
    );
    module.push_str(&m.funcs);
    module.push_str(&main);
    Ok(module)
}

fn compile_main(stmts: &[TStmt], m: &mut Module) -> Result<String> {
    let mut g = Gen::new();
    for s in stmts {
        match s {
            TStmt::Let(_, te) if matches!(te.kind, TExprKind::Lambda(..)) => {}
            TStmt::Let(name, rhs) => {
                let v = g.expr(rhs, m)?;
                g.bind_local(name, &v, &rhs.ty)?;
            }
            TStmt::Expr(e) => {
                let v = g.expr(e, m)?;
                emit_print(&mut g, &v, &e.ty)?;
            }
            TStmt::Type(_) | TStmt::Impl(_) => {}
        }
    }
    g.line("ret i32 0");
    Ok(format!("define i32 @main() {{\n{}}}\n", g.out))
}

fn emit_print(g: &mut Gen, operand: &str, ty: &Type) -> Result<()> {
    match ty {
        Type::Int | Type::Long => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.di, i64 {})",
                r, operand
            ));
        }
        Type::Float => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.df, double {})",
                r, operand
            ));
        }
        Type::Bool => {
            let s = g.fresh();
            g.line(&format!("%s{} = select i1 {}, ptr @.t, ptr @.f", s, operand));
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.ds, ptr %s{})",
                r, s
            ));
        }
        Type::Str => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.ds, ptr {})",
                r, operand
            ));
        }
        other => return unsupported(format!("出力できない型 {}", other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::Checker;
    use crate::lexer::tokenize;
    use crate::parser::Parser;

    fn try_ir(src: &str) -> Result<String> {
        let tokens = tokenize(src).unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let typed = Checker::new().check_program(&program).unwrap();
        compile_program(&typed)
    }
    fn ir(src: &str) -> String {
        try_ir(src).unwrap()
    }

    #[test]
    fn 関数と呼び出しと算術を出力する() {
        let out = ir("let f = fn(x: Int) -> Int { x + 1 }; f(2)");
        assert!(out.contains("define i64 @f(ptr %env, i64 %arg0)"));
        assert!(out.contains("add i64"));
        assert!(out.contains("call i64 @f(ptr null"));
        assert!(out.contains("define i32 @main()"));
    }

    #[test]
    fn 再帰関数とifを出力する() {
        let out = ir("let fact = fn(n: Int) -> Int { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)");
        assert!(out.contains("define i64 @fact"));
        assert!(out.contains("icmp eq i64"));
        assert!(out.contains("mul i64"));
    }

    #[test]
    fn クロージャはmallocとキャプチャを出力する() {
        let out = ir("let mk = fn(n: Int) { fn(x: Int) -> Int { x + n } }; 0");
        assert!(out.contains("@malloc"));
        assert!(out.contains("insertvalue"));
    }

    #[test]
    fn 文字列の連結を出力する() {
        let out = ir(r#""a" <> "b""#);
        assert!(out.contains("@awa_concat"));
        assert!(out.contains("constant")); // 文字列リテラルのグローバル
    }

    #[test]
    fn adtの構築とcaseを出力する() {
        let out = ir(
            "type Opt(a) { Some(a); None }; \
             let f = fn(o: Opt(Int), d: Int) -> Int { case o { Some(x) -> x; None -> d } }; \
             f(Some(1), 0)",
        );
        assert!(out.contains("@malloc")); // 構築でヒープ確保
        assert!(out.contains("load i32")); // case で tag を読む
        assert!(out.contains("unreachable")); // 網羅後のフォールスルー
    }
}
