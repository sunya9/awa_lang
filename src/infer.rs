use std::collections::HashMap;

use crate::ast::{Expr, Stmt};
use crate::error::AwaError;
use crate::token::Token;
use crate::types::{Subst, Type, VarGen, apply, unify};

// ===== 型推論（Algorithm W / 単相 Hindley-Milner）=====
// AST をたどって式の型を計算し、矛盾があれば unify が CannotUnify などを返す。
// 多相（let 多相 = generalize/instantiate）は今は省略。同じ関数を Num と Bool の
// 両方で使うと型エラーになるが、教科書的な単相 HM として十分動く。
// 必要になったら generalize/instantiate をここに足す。

// 型環境: 変数名 → 型。値の Env（名前→Value）の型版。
pub type TypeEnv = HashMap<String, Type>;

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

// let 束縛の型推論。再帰関数のために、本体を見る前に名前を fresh 型変数で
// 先に束縛しておく（letrec 的）。これにより `let fact = fn(n){ ... fact(n-1) ... }`
// の本体内で fact を参照でき、推論後に「先に置いた型変数」と「本体から推論した型」を
// unify して辻褄を合わせる。eval 側の「名前を刻んで自己注入」と対になる仕組み。
fn infer_let(
    name: &str,
    expr: &Expr,
    env: &mut TypeEnv,
    subst: &mut Subst,
    vargen: &mut VarGen,
) -> Result<Type, AwaError> {
    let placeholder = vargen.fresh();
    env.insert(name.to_string(), placeholder.clone());
    let t = infer_expr(expr, env, subst, vargen)?;
    unify(subst, &placeholder, &t)?;
    let resolved = apply(subst, &placeholder);
    // 推論で確定した型で上書きしておく（後続の文がこの型で参照する）。
    env.insert(name.to_string(), resolved.clone());
    Ok(resolved)
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
        // 変数は型環境を引く。無ければ評価時と同じ UndefinedVariable。
        Expr::Var(name) => env
            .get(name)
            .cloned()
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
                body_env.insert(p.clone(), tv.clone());
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
}
