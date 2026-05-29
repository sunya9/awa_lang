use std::rc::Rc;

use crate::ast::{Pattern, TExpr, TExprKind, TMethod, TStmt, TypeDef};
use crate::error::{AwaError, LocatedError};
use crate::span::Span;
use crate::token::Token;
use crate::value::{Closure, Env, Value, type_name};

// ===== 評価（Evaluator）=====

// AwaError（種類）に span を添えて LocatedError にする小ヘルパ。
// 実行時エラーを「その式の位置」で包むのに使う。
fn at<T>(r: Result<T, AwaError>, span: Span) -> Result<T, LocatedError> {
    r.map_err(|e| LocatedError::at(e, span))
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

pub fn eval_expr(expr: &TExpr, env: &Env) -> Result<Value, LocatedError> {
    let span = expr.span;
    match &expr.kind {
        TExprKind::Int(n) => Ok(Value::Int(*n)),
        TExprKind::Long(n) => Ok(Value::Long(*n)),
        TExprKind::Float(n) => Ok(Value::Float(*n)),
        TExprKind::Str(s) => Ok(Value::Str(s.clone())),
        TExprKind::Bool(b) => Ok(Value::Bool(*b)),
        // 値はもう Copy ではないので .cloned() で複製する。
        TExprKind::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| LocatedError::at(AwaError::UndefinedVariable(name.clone()), span)),
        // 単項マイナス。型検査済みなので数値のはず。
        TExprKind::Neg(operand) => match eval_expr(operand, env)? {
            Value::Int(n) => Ok(Value::Int(n.wrapping_neg())),
            Value::Long(n) => Ok(Value::Long(n.wrapping_neg())),
            Value::Float(n) => Ok(Value::Float(-n)),
            _ => unreachable!("Neg のオペランドは数値（型検査済み）"),
        },
        TExprKind::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            at(eval_binop(l, op, r), span)
        }
        // if は選ばれた腕だけを評価する（短絡評価）。
        TExprKind::If(cond, then_branch, else_branch) => {
            let c = eval_expr(cond, env)?;
            if at(as_bool(c), span)? {
                eval_expr(then_branch, env)
            } else {
                eval_expr(else_branch, env)
            }
        }
        // 無名関数を評価すると Closure（関数 + 現在の環境のコピー）になる。
        // env.clone() がキャプチャの実体。呼び出し時にこの env をベースに使う。
        // 引数名は型付きノードに既に名前だけで入っている。
        TExprKind::Lambda(params, body) => Ok(Value::Func(Rc::new(Closure {
            params: params.clone(),
            body: (**body).clone(),
            env: env.clone(),
            name: None, // 束縛時に let 側で名前を付け直す（再帰のため）
        }))),
        // 関数呼び出し。
        TExprKind::Call(callee, args) => {
            // 1. 呼ぶ対象を評価する。構築子なら Data を組み立てて返す。
            let callee_val = eval_expr(callee, env)?;
            let closure = match callee_val {
                Value::Func(c) => c,
                // 構築子適用: 引数を評価して Data を作る。
                Value::Ctor { name, arity } => {
                    if args.len() != arity {
                        return Err(LocatedError::at(
                            AwaError::ArityMismatch {
                                expected: arity,
                                found: args.len(),
                            },
                            span,
                        ));
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
                // 組込関数（変換など）。引数を評価して native_call へ。
                Value::Native(name) => {
                    let mut arg_values = Vec::with_capacity(args.len());
                    for arg in args {
                        arg_values.push(eval_expr(arg, env)?);
                    }
                    return native_call(&name, arg_values);
                }
                other => {
                    return Err(LocatedError::at(
                        AwaError::NotCallable(type_name(&other)),
                        span,
                    ));
                }
            };
            // 2. 引数の個数チェック。
            if args.len() != closure.params.len() {
                return Err(LocatedError::at(
                    AwaError::ArityMismatch {
                        expected: closure.params.len(),
                        found: args.len(),
                    },
                    span,
                ));
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
        TExprKind::Block(stmts) => {
            let mut block_env = env.clone();
            let mut value = Value::Unit;
            for stmt in stmts {
                value = match stmt {
                    TStmt::Let(name, e) => {
                        let v = bind_name(eval_expr(e, &block_env)?, name);
                        block_env.insert(name.clone(), v);
                        Value::Unit // let 文はブロック値に寄与しない
                    }
                    TStmt::Type(def) => {
                        register_ctors(def, &mut block_env);
                        Value::Unit // 型宣言もブロック値に寄与しない
                    }
                    TStmt::Impl(ms) => {
                        register_methods(ms, &mut block_env);
                        Value::Unit // impl もブロック値に寄与しない
                    }
                    TStmt::Expr(e) => eval_expr(e, &block_env)?,
                };
            }
            Ok(value)
        }
        // パターンマッチ。主語を評価し、腕を上から試して最初に当たったものを評価する。
        TExprKind::Case(subject, arms) => {
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
            Err(LocatedError::at(AwaError::NoMatch, span))
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
        Pattern::Int(n) => match value {
            Value::Int(v) if v == n => Some(Vec::new()),
            _ => None,
        },
        Pattern::Str(s) => match value {
            Value::Str(v) if v == s => Some(Vec::new()),
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

// impl の各メソッドをマングル名（型頭#メソッド）のクロージャとして環境に登録する。
// name を刻むので自己再帰メソッドは呼び出し時に自分自身を注入できる（let と同じ仕組み）。
// メソッド呼び出しはチェッカが Call(Var(マングル名), [recv, ...]) へ lower 済みなので、
// あとは通常の関数として呼ばれる。
fn register_methods(methods: &[TMethod], env: &mut Env) {
    for m in methods {
        let closure = Closure {
            params: m.params.clone(),
            body: m.body.clone(),
            env: env.clone(),
            name: Some(m.mangled.clone()),
        };
        env.insert(m.mangled.clone(), Value::Func(Rc::new(closure)));
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

// 二項演算の評価。型検査済みなので両辺は同じ型（算術/比較は数値同型、連結は String）。
// 整数はラップ、Float は IEEE。整数の 0 除算のみ DivisionByZero。
fn eval_binop(l: Value, op: &Token, r: Value) -> Result<Value, AwaError> {
    match op {
        Token::Plus => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_add(b))),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a.wrapping_add(b))),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            _ => unreachable!("算術は数値同型（型検査済み）"),
        },
        Token::Minus => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_sub(b))),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a.wrapping_sub(b))),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
            _ => unreachable!(),
        },
        Token::Star => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_mul(b))),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a.wrapping_mul(b))),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a * b)),
            _ => unreachable!(),
        },
        Token::Slash => match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    Err(AwaError::DivisionByZero)
                } else {
                    Ok(Value::Int(a.wrapping_div(b)))
                }
            }
            (Value::Long(a), Value::Long(b)) => {
                if b == 0 {
                    Err(AwaError::DivisionByZero)
                } else {
                    Ok(Value::Long(a.wrapping_div(b)))
                }
            }
            // Float の 0.0 除算は IEEE（inf/NaN）。エラーにしない。
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a / b)),
            _ => unreachable!(),
        },
        // 文字列連結。
        Token::Concat => match (l, r) {
            (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
            _ => unreachable!("<> は String 同士（型検査済み）"),
        },
        // 比較は数値同型。
        Token::Lt => Ok(Value::Bool(num_cmp(&l, &r).is_lt())),
        Token::Gt => Ok(Value::Bool(num_cmp(&l, &r).is_gt())),
        Token::Le => Ok(Value::Bool(num_cmp(&l, &r).is_le())),
        Token::Ge => Ok(Value::Bool(num_cmp(&l, &r).is_ge())),
        Token::EqEq => Ok(Value::Bool(l == r)),
        Token::NotEq => Ok(Value::Bool(l != r)),
        _ => unreachable!(),
    }
}

// 数値同型の大小比較（型検査済みなので Int/Long/Float のいずれか同士）。
fn num_cmp(l: &Value, r: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Long(a), Value::Long(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        _ => unreachable!("比較は数値同型（型検査済み）"),
    }
}

// ===== 組込関数（最小限の型変換）=====
// 名前で識別される native 関数。型検査済みなので引数の型は保証されている。
fn native_call(name: &str, args: Vec<Value>) -> Result<Value, LocatedError> {
    let arg = args.into_iter().next();
    let v = match (name, arg) {
        ("int_to_float", Some(Value::Int(n))) => Value::Float(n as f64),
        ("float_to_int", Some(Value::Float(n))) => Value::Int(n as i64),
        ("int_to_long", Some(Value::Int(n))) => Value::Long(n),
        ("long_to_int", Some(Value::Long(n))) => Value::Int(n),
        ("int_to_string", Some(Value::Int(n))) => Value::Str(n.to_string()),
        ("float_to_string", Some(Value::Float(n))) => Value::Str(n.to_string()),
        _ => unreachable!("組込関数の引数は型検査済み: {}", name),
    };
    Ok(v)
}

// 組込関数を仕込んだ初期環境。run/REPL の値環境はこれから始める。
// （型側のスキームは Checker::new が seed する。名前は両者で一致させること。）
pub fn builtins_env() -> Env {
    let mut env = Env::new();
    for name in [
        "int_to_float",
        "float_to_int",
        "int_to_long",
        "long_to_int",
        "int_to_string",
        "float_to_string",
    ] {
        env.insert(name.to_string(), Value::Native(name.to_string()));
    }
    env
}

// プログラム（文の並び）を順に評価し、各文の値を順に返す。
// env は呼び出し側が所有して渡す（REPL は行をまたいで使い回す）。
pub fn eval_program(stmts: &[TStmt], env: &mut Env) -> Result<Vec<Value>, LocatedError> {
    let mut results = Vec::new();
    for stmt in stmts {
        match stmt {
            TStmt::Let(name, expr) => {
                let value = bind_name(eval_expr(expr, env)?, name);
                env.insert(name.clone(), value.clone());
                results.push(value);
            }
            TStmt::Expr(expr) => results.push(eval_expr(expr, env)?),
            // 型宣言は構築子を登録するだけ。結果行は出さない。
            TStmt::Type(def) => register_ctors(def, env),
            // impl はメソッドを登録するだけ。結果行は出さない。
            TStmt::Impl(ms) => register_methods(ms, env),
        };
    }
    Ok(results)
}
