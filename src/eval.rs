use std::rc::Rc;

use crate::ast::{Expr, Pattern, Stmt, TypeDef};
use crate::error::AwaError;
use crate::token::Token;
use crate::value::{Closure, Env, Value, type_name};

// ===== 評価（Evaluator）=====

// 値から f64 を取り出す。数値でなければ型エラー（算術・比較は数値専用）。
fn as_num(v: Value) -> Result<f64, AwaError> {
    match v {
        Value::Num(n) => Ok(n),
        other => Err(AwaError::TypeError {
            expected: "Num",
            found: type_name(&other),
        }),
    }
}

// 値から bool を取り出す。真偽値でなければ型エラー（if の条件は Bool 専用）。
fn as_bool(v: Value) -> Result<bool, AwaError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(AwaError::TypeError {
            expected: "Bool",
            found: type_name(&other),
        }),
    }
}

// 関数値に名前を刻む（再帰のため）。関数以外はそのまま返す。
// let で束縛するときに使い、呼び出し時に「名前→自分自身」を注入できるようにする。
fn bind_name(value: Value, name: &str) -> Value {
    match value {
        Value::Func(c) => Value::Func(Rc::new(Closure {
            params: c.params.clone(),
            body: c.body.clone(),
            env: c.env.clone(),
            name: Some(name.to_string()),
        })),
        other => other,
    }
}

pub fn eval_expr(expr: &Expr, env: &Env) -> Result<Value, AwaError> {
    match expr {
        Expr::Num(n) => Ok(Value::Num(*n)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        // 値はもう Copy ではないので .cloned() で複製する。
        // Num/Bool は安価、Func は Rc のカウント増加で済む。
        Expr::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| AwaError::UndefinedVariable(name.clone())),
        Expr::Neg(operand) => Ok(Value::Num(-as_num(eval_expr(operand, env)?)?)),
        Expr::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            eval_binop(l, op, r)
        }
        // if は選ばれた腕だけを評価する（短絡評価）。
        Expr::If(cond, then_branch, else_branch) => {
            if as_bool(eval_expr(cond, env)?)? {
                eval_expr(then_branch, env)
            } else {
                eval_expr(else_branch, env)
            }
        }
        // 無名関数を評価すると Closure（関数 + 現在の環境のコピー）になる。
        // env.clone() がキャプチャの実体。呼び出し時にこの env をベースに使う。
        // 型注釈は実行時には無視し、引数名だけ取り出す。
        Expr::Lambda(params, _ret, body) => Ok(Value::Func(Rc::new(Closure {
            params: params.iter().map(|(name, _)| name.clone()).collect(),
            body: (**body).clone(),
            env: env.clone(),
            name: None, // 束縛時に let 側で名前を付け直す（再帰のため）
        }))),
        // 関数呼び出し。
        Expr::Call(callee, args) => {
            // 1. 呼ぶ対象を評価する。構築子なら Data を組み立てて返す。
            let callee_val = eval_expr(callee, env)?;
            let closure = match callee_val {
                Value::Func(c) => c,
                // 構築子適用: 引数を評価して Data を作る。
                Value::Ctor { name, arity } => {
                    if args.len() != arity {
                        return Err(AwaError::ArityMismatch {
                            expected: arity,
                            found: args.len(),
                        });
                    }
                    let mut arg_values = Vec::with_capacity(args.len());
                    for arg in args {
                        arg_values.push(eval_expr(arg, env)?);
                    }
                    return Ok(Value::Data {
                        ctor: name,
                        args: arg_values,
                    });
                }
                other => return Err(AwaError::NotCallable(type_name(&other))),
            };
            // 2. 引数の個数チェック。
            if args.len() != closure.params.len() {
                return Err(AwaError::ArityMismatch {
                    expected: closure.params.len(),
                    found: args.len(),
                });
            }
            // 3. 各引数を「呼び出し側の env」で評価する（ここは呼び出し側スコープ）。
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr(arg, env)?);
            }
            // 本体を評価する環境を作る。ベースはキャプチャした closure.env
            // （レキシカルスコープ）で、呼び出し側 env ではない。
            let mut call_env = closure.env.clone();
            // 再帰のため、名前付き関数なら「名前→自分自身」を call_env に入れる。
            if let Some(name) = &closure.name {
                call_env.insert(name.clone(), Value::Func(closure.clone()));
            }
            // 仮引数に実引数を束縛する。
            for (param, value) in closure.params.iter().zip(arg_values) {
                call_env.insert(param.clone(), value);
            }
            eval_expr(&closure.body, &call_env)
        }
        // ブロックは新しいスコープ。親 env をコピーして始め、抜けると破棄する
        // （ブロック内の let は外に漏れない）。値は最後の文が式ならその値、
        // そうでなければ（空 or 末尾が let）Unit。
        Expr::Block(stmts) => {
            let mut block_env = env.clone();
            let mut value = Value::Unit;
            for stmt in stmts {
                value = match stmt {
                    Stmt::Let(name, _, e) => {
                        let v = bind_name(eval_expr(e, &block_env)?, name);
                        block_env.insert(name.clone(), v);
                        Value::Unit // let 文はブロック値に寄与しない
                    }
                    Stmt::Type(def) => {
                        register_ctors(def, &mut block_env);
                        Value::Unit // 型宣言もブロック値に寄与しない
                    }
                    Stmt::Expr(e) => eval_expr(e, &block_env)?,
                };
            }
            Ok(value)
        }
        // パターンマッチ。主語を評価し、腕を上から試して最初に当たったものを評価する。
        Expr::Case(subject, arms) => {
            let value = eval_expr(subject, env)?;
            for (pattern, body) in arms {
                if let Some(bindings) = match_pattern(pattern, &value) {
                    // 当たった腕の束縛を環境に入れて本体を評価する。
                    let mut arm_env = env.clone();
                    for (name, v) in bindings {
                        arm_env.insert(name, v);
                    }
                    return eval_expr(body, &arm_env);
                }
            }
            // どの腕にも当たらなかった（網羅性チェックは Phase 3b で静的に防ぐ）。
            Err(AwaError::NoMatch)
        }
    }
}

// パターンを値に照合する。当たれば束縛（変数名→値）のリストを Some で返す。
// 当たらなければ None。Ctor は ctor 名と arity が一致したとき、部分パターンを
// 引数に再帰照合して束縛を連結する。
fn match_pattern(pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
    match pattern {
        Pattern::Wildcard => Some(Vec::new()),
        Pattern::Var(name) => Some(vec![(name.clone(), value.clone())]),
        Pattern::Bool(b) => match value {
            Value::Bool(v) if v == b => Some(Vec::new()),
            _ => None,
        },
        Pattern::Num(n) => match value {
            Value::Num(v) if v == n => Some(Vec::new()),
            _ => None,
        },
        Pattern::Ctor(name, subs) => match value {
            Value::Data { ctor, args } if ctor == name && args.len() == subs.len() => {
                let mut bindings = Vec::new();
                for (sub, arg) in subs.iter().zip(args.iter()) {
                    bindings.extend(match_pattern(sub, arg)?);
                }
                Some(bindings)
            }
            _ => None,
        },
        // or パターン: いずれかの選択肢に当たればその束縛を返す（束縛の整合性は型検査で保証済み）。
        Pattern::Or(alts) => alts.iter().find_map(|alt| match_pattern(alt, value)),
    }
}

// 型宣言の各構築子を環境に登録する。ヌル構築子は直接 Data 値、
// 引数を取る構築子は Ctor 値（呼び出し時に Data を組み立てる）。
fn register_ctors(def: &TypeDef, env: &mut Env) {
    for variant in &def.variants {
        let value = if variant.fields.is_empty() {
            Value::Data {
                ctor: variant.name.clone(),
                args: Vec::new(),
            }
        } else {
            Value::Ctor {
                name: variant.name.clone(),
                arity: variant.fields.len(),
            }
        };
        env.insert(variant.name.clone(), value);
    }
}

// 二項演算の評価。算術は Num→Num、比較は Num→Bool、等価は同じ型同士→Bool。
fn eval_binop(l: Value, op: &Token, r: Value) -> Result<Value, AwaError> {
    match op {
        Token::Plus => Ok(Value::Num(as_num(l)? + as_num(r)?)),
        Token::Minus => Ok(Value::Num(as_num(l)? - as_num(r)?)),
        Token::Star => Ok(Value::Num(as_num(l)? * as_num(r)?)),
        Token::Slash => {
            let (a, b) = (as_num(l)?, as_num(r)?);
            if b == 0.0 {
                Err(AwaError::DivisionByZero)
            } else {
                Ok(Value::Num(a / b))
            }
        }
        Token::Lt => Ok(Value::Bool(as_num(l)? < as_num(r)?)),
        Token::Gt => Ok(Value::Bool(as_num(l)? > as_num(r)?)),
        Token::Le => Ok(Value::Bool(as_num(l)? <= as_num(r)?)),
        Token::Ge => Ok(Value::Bool(as_num(l)? >= as_num(r)?)),
        Token::EqEq => Ok(Value::Bool(values_equal(l, r)?)),
        Token::NotEq => Ok(Value::Bool(!values_equal(l, r)?)),
        _ => unreachable!(),
    }
}

fn values_equal(l: Value, r: Value) -> Result<bool, AwaError> {
    match (l, r) {
        (Value::Num(a), Value::Num(b)) => Ok(a == b),
        (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
        (Value::Unit, Value::Unit) => Ok(true),
        (l, r) => Err(AwaError::TypeError {
            expected: type_name(&l),
            found: type_name(&r),
        }),
    }
}

// プログラム（文の並び）を順に評価し、各文の値を順に返す。
// env は呼び出し側が所有して渡す（REPL は行をまたいで使い回す）。
pub fn eval_program(stmts: &[Stmt], env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let mut results = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, _, expr) => {
                let value = bind_name(eval_expr(expr, env)?, name);
                env.insert(name.clone(), value.clone());
                results.push(value);
            }
            Stmt::Expr(expr) => results.push(eval_expr(expr, env)?),
            // 型宣言は構築子を登録するだけ。結果行は出さない。
            Stmt::Type(def) => register_ctors(def, env),
        };
    }
    Ok(results)
}
