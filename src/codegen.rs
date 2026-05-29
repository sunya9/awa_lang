use std::collections::HashMap;

use crate::ast::{TExpr, TExprKind, TStmt};
use crate::error::AwaError;
use crate::token::Token;
use crate::types::Type;

// ===== LLVM コード生成（バックエンド・マイルストーン1）=====
// 型付き AST から LLVM IR テキスト(.ll)を生成する。clang file.ll -o exe でネイティブ化。
// 対応サブセット: Int(i64)/Bool(i1)、算術・比較・if・let・トップレベル非キャプチャ関数・
// 呼び出し・再帰。未対応の構造/型に当たったら AwaError::Unsupported を返す（誤コンパイルしない）。
//
// SSA は alloca＋load/store で組み（phi 回避）、各基本ブロックは out へ順次 append する。

type Result<T> = std::result::Result<T, AwaError>;

fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(AwaError::Unsupported(what.into()))
}

// awa の型 → LLVM 型。マイルストーン1 は Int/Bool のみ。
fn llvm_ty(ty: &Type) -> Result<&'static str> {
    match ty {
        Type::Int => Ok("i64"),
        Type::Bool => Ok("i1"),
        other => unsupported(format!("型 {}", other)),
    }
}

// トップレベル関数のシグネチャ。
struct FnSig {
    params: Vec<Type>,
    ret: Type,
}

// 1 関数本体の生成状態。
struct Gen<'a> {
    out: String,                              // 命令を溜める
    reg: usize,                               // レジスタ/ラベルの採番
    locals: HashMap<String, (String, Type)>, // 変数名 → (alloca の ptr レジスタ, 型)
    funcs: &'a HashMap<String, FnSig>,        // 呼び出せるトップレベル関数
}

impl<'a> Gen<'a> {
    fn new(funcs: &'a HashMap<String, FnSig>) -> Self {
        Gen {
            out: String::new(),
            reg: 0,
            locals: HashMap::new(),
            funcs,
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

    // ローカル変数を alloca して値を格納し、名前を登録する。
    fn bind_local(&mut self, name: &str, operand: &str, ty: &Type) -> Result<()> {
        let llty = llvm_ty(ty)?;
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, llty));
        self.line(&format!("store {} {}, ptr {}", llty, operand, slot));
        self.locals.insert(name.to_string(), (slot, ty.clone()));
        Ok(())
    }

    // 式をコンパイルし、結果の LLVM オペランド（即値 or レジスタ）を返す。
    fn expr(&mut self, e: &TExpr) -> Result<String> {
        match &e.kind {
            TExprKind::Int(n) => Ok(n.to_string()),
            TExprKind::Bool(b) => Ok(if *b { "1".into() } else { "0".into() }),
            TExprKind::Var(name) => {
                if let Some((slot, ty)) = self.locals.get(name).cloned() {
                    let llty = llvm_ty(&ty)?;
                    let r = format!("%t{}", self.fresh());
                    self.line(&format!("{} = load {}, ptr {}", r, llty, slot));
                    Ok(r)
                } else if self.funcs.contains_key(name) {
                    unsupported(format!("関数を値として使う: {}", name))
                } else {
                    // params 以外の自由変数（クロージャのキャプチャ）。
                    unsupported(format!("自由変数（キャプチャ）: {}", name))
                }
            }
            TExprKind::Neg(x) => {
                let v = self.expr(x)?;
                let r = format!("%t{}", self.fresh());
                self.line(&format!("{} = sub i64 0, {}", r, v));
                Ok(r)
            }
            TExprKind::BinOp(l, op, r) => self.binop(l, op, r),
            TExprKind::If(c, t, e) => self.if_expr(c, t, e),
            TExprKind::Call(callee, args) => self.call(callee, args),
            TExprKind::Block(stmts) => self.block(stmts),
            TExprKind::Lambda(..) => unsupported("ネストした関数/クロージャ"),
            TExprKind::Case(..) => unsupported("case（パターンマッチ）"),
            TExprKind::Long(_) | TExprKind::Float(_) | TExprKind::Str(_) => {
                unsupported(format!("型 {}", e.ty))
            }
        }
    }

    fn binop(&mut self, l: &TExpr, op: &Token, r: &TExpr) -> Result<String> {
        let lv = self.expr(l)?;
        let rv = self.expr(r)?;
        let r_reg = format!("%t{}", self.fresh());
        let instr = match op {
            Token::Plus => "add i64",
            Token::Minus => "sub i64",
            Token::Star => "mul i64",
            Token::Slash => "sdiv i64", // 注: 0 除算は v1 では未チェック（UB）
            // 比較。オペランドの LLVM 型は l の型から（Int/Bool）。
            Token::Lt => return self.icmp("slt", l, &lv, &rv),
            Token::Gt => return self.icmp("sgt", l, &lv, &rv),
            Token::Le => return self.icmp("sle", l, &lv, &rv),
            Token::Ge => return self.icmp("sge", l, &lv, &rv),
            Token::EqEq => return self.icmp("eq", l, &lv, &rv),
            Token::NotEq => return self.icmp("ne", l, &lv, &rv),
            Token::Concat => return unsupported("文字列連結 <>"),
            other => unreachable!("二項演算子でないトークン: {:?}", other),
        };
        self.line(&format!("{} = {} {}, {}", r_reg, instr, lv, rv));
        Ok(r_reg)
    }

    fn icmp(&mut self, pred: &str, operand_expr: &TExpr, lv: &str, rv: &str) -> Result<String> {
        let llty = llvm_ty(&operand_expr.ty)?;
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = icmp {} {} {}, {}", r, pred, llty, lv, rv));
        Ok(r)
    }

    fn if_expr(&mut self, c: &TExpr, t: &TExpr, e: &TExpr) -> Result<String> {
        let res_ty = llvm_ty(&t.ty)?;
        // 結果スロットを先に確保。
        let slot = format!("%v{}", self.fresh());
        self.line(&format!("{} = alloca {}", slot, res_ty));
        let cond = self.expr(c)?;
        let id = self.fresh();
        let (then_l, else_l, merge_l) =
            (format!("then{id}"), format!("else{id}"), format!("merge{id}"));
        self.line(&format!(
            "br i1 {}, label %{}, label %{}",
            cond, then_l, else_l
        ));
        // then
        self.label(&then_l);
        let tv = self.expr(t)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, tv, slot));
        self.line(&format!("br label %{}", merge_l));
        // else
        self.label(&else_l);
        let ev = self.expr(e)?;
        self.line(&format!("store {} {}, ptr {}", res_ty, ev, slot));
        self.line(&format!("br label %{}", merge_l));
        // merge
        self.label(&merge_l);
        let r = format!("%t{}", self.fresh());
        self.line(&format!("{} = load {}, ptr {}", r, res_ty, slot));
        Ok(r)
    }

    fn call(&mut self, callee: &TExpr, args: &[TExpr]) -> Result<String> {
        // callee はトップレベル関数名でなければならない（値としての関数は未対応）。
        let TExprKind::Var(name) = &callee.kind else {
            return unsupported("間接呼び出し（関数値）");
        };
        let Some(sig) = self.funcs.get(name) else {
            return unsupported(format!("ローカル関数の呼び出し: {}", name));
        };
        if sig.params.len() != args.len() {
            return unsupported("引数個数不一致");
        }
        let ret = llvm_ty(&sig.ret)?.to_string();
        // 引数を評価（先に型文字列を作ってから可変借用を解く）。
        let param_tys: Vec<String> = sig
            .params
            .iter()
            .map(|t| llvm_ty(t).map(|s| s.to_string()))
            .collect::<Result<_>>()?;
        let mut parts = Vec::with_capacity(args.len());
        for (a, pty) in args.iter().zip(param_tys.iter()) {
            let v = self.expr(a)?;
            parts.push(format!("{} {}", pty, v));
        }
        let r = format!("%t{}", self.fresh());
        self.line(&format!(
            "{} = call {} @{}({})",
            r,
            ret,
            name,
            parts.join(", ")
        ));
        Ok(r)
    }

    fn block(&mut self, stmts: &[TStmt]) -> Result<String> {
        // 新スコープ（簡易: 既存 locals を保存して復元）。
        let saved = self.locals.clone();
        let mut last = String::new();
        for s in stmts {
            match s {
                TStmt::Let(name, rhs) => {
                    if matches!(rhs.kind, TExprKind::Lambda(..)) {
                        return unsupported("ブロック内の関数定義");
                    }
                    let v = self.expr(rhs)?;
                    self.bind_local(name, &v, &rhs.ty)?;
                    last = String::new();
                }
                TStmt::Expr(e) => last = self.expr(e)?,
                TStmt::Type(_) | TStmt::Impl(_) => {
                    return unsupported("ブロック内の type/impl");
                }
            }
        }
        self.locals = saved;
        if last.is_empty() {
            unsupported("値を持たないブロック")
        } else {
            Ok(last)
        }
    }
}

// プログラム全体を LLVM IR テキストへ。
pub fn compile_program(stmts: &[TStmt]) -> Result<String> {
    // パス1: トップレベル関数のシグネチャを集める（前方参照・再帰のため）。
    let mut funcs: HashMap<String, FnSig> = HashMap::new();
    for s in stmts {
        if let TStmt::Let(name, te) = s {
            if let TExprKind::Lambda(_, _) = &te.kind {
                let Type::Fun(params, ret) = &te.ty else {
                    return unsupported("関数の型が解決できない");
                };
                funcs.insert(
                    name.clone(),
                    FnSig {
                        params: params.clone(),
                        ret: (**ret).clone(),
                    },
                );
            }
        }
    }

    let mut module = String::new();
    // target triple はあえて出力しない（clang がホスト既定を使う＝上書き警告を避ける）。
    module.push_str("declare i32 @printf(ptr, ...)\n");
    module.push_str("@.di = private unnamed_addr constant [6 x i8] c\"%lld\\0A\\00\"\n");
    module.push_str("@.ds = private unnamed_addr constant [4 x i8] c\"%s\\0A\\00\"\n");
    module.push_str("@.t = private unnamed_addr constant [5 x i8] c\"true\\00\"\n");
    module.push_str("@.f = private unnamed_addr constant [6 x i8] c\"false\\00\"\n\n");

    // パス2: 各関数定義を出力。
    for s in stmts {
        if let TStmt::Let(name, te) = s {
            if let TExprKind::Lambda(param_names, body) = &te.kind {
                module.push_str(&compile_fn(name, param_names, body, &funcs)?);
                module.push('\n');
            }
        }
    }

    // main: 関数定義でないトップレベル文を実行し、Expr 文の結果を printf。
    module.push_str(&compile_main(stmts, &funcs)?);
    Ok(module)
}

fn compile_fn(
    name: &str,
    param_names: &[String],
    body: &TExpr,
    funcs: &HashMap<String, FnSig>,
) -> Result<String> {
    let sig = &funcs[name];
    let ret = llvm_ty(&sig.ret)?;
    let mut g = Gen::new(funcs);
    // 引数を alloca にコピー（Var の load を一様にする）。
    let mut header_params = Vec::new();
    let mut arg_index = 0;
    for (pname, pty) in param_names.iter().zip(sig.params.iter()) {
        let llty = llvm_ty(pty)?;
        header_params.push(format!("{} %arg{}", llty, arg_index));
        g.bind_local(pname, &format!("%arg{}", arg_index), pty)?;
        arg_index += 1;
    }
    let result = g.expr(body)?;
    g.line(&format!("ret {} {}", ret, result));
    Ok(format!(
        "define {} @{}({}) {{\n{}}}\n",
        ret,
        name,
        header_params.join(", "),
        g.out
    ))
}

fn compile_main(stmts: &[TStmt], funcs: &HashMap<String, FnSig>) -> Result<String> {
    let mut g = Gen::new(funcs);
    for s in stmts {
        match s {
            // 関数定義はパス2 で出力済み。スキップ。
            TStmt::Let(_, te) if matches!(te.kind, TExprKind::Lambda(..)) => {}
            // トップレベルの値 let は main のローカルに束縛。
            TStmt::Let(name, rhs) => {
                let v = g.expr(rhs)?;
                g.bind_local(name, &v, &rhs.ty)?;
            }
            // トップレベル式は評価して printf。
            TStmt::Expr(e) => {
                let v = g.expr(e)?;
                emit_print(&mut g, &v, &e.ty)?;
            }
            TStmt::Type(_) | TStmt::Impl(_) => {}
        }
    }
    g.line("ret i32 0");
    Ok(format!("define i32 @main() {{\n{}}}\n", g.out))
}

// 値を型に応じて printf する。
fn emit_print(g: &mut Gen, operand: &str, ty: &Type) -> Result<()> {
    match ty {
        Type::Int => {
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.di, i64 {})",
                r, operand
            ));
        }
        Type::Bool => {
            // i1 を "true"/"false" の文字列ポインタへ select して printf。
            let s = g.fresh();
            g.line(&format!(
                "%s{} = select i1 {}, ptr @.t, ptr @.f",
                s, operand
            ));
            let r = g.fresh();
            g.line(&format!(
                "%p{} = call i32 (ptr, ...) @printf(ptr @.ds, ptr %s{})",
                r, s
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
        assert!(out.contains("define i64 @f(i64 %arg0)"));
        assert!(out.contains("add i64"));
        assert!(out.contains("call i64 @f("));
        assert!(out.contains("define i32 @main()"));
        assert!(out.contains("@printf"));
    }

    #[test]
    fn 再帰関数とifを出力する() {
        let out = ir("let fact = fn(n: Int) -> Int { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)");
        assert!(out.contains("define i64 @fact"));
        assert!(out.contains("icmp eq i64"));
        assert!(out.contains("mul i64"));
        assert!(out.contains("br i1"));
    }

    #[test]
    fn 文字列は未対応() {
        assert!(matches!(try_ir(r#""hi""#), Err(AwaError::Unsupported(_))));
    }

    #[test]
    fn クロージャは未対応() {
        // 内側の fn が外側の n をキャプチャ → codegen 未対応。
        assert!(matches!(
            try_ir("let mk = fn(n: Int) { fn(x: Int) -> Int { x + n } }; 0"),
            Err(AwaError::Unsupported(_))
        ));
    }

    #[test]
    fn adtは未対応() {
        assert!(matches!(
            try_ir("type Opt(a) { Some(a); None }; Some(1)"),
            Err(AwaError::Unsupported(_))
        ));
    }
}
