use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use crate::ast::{Pattern, TExpr, TExprKind, TMethod, TStmt, TypeDef};
use crate::error::{AwaError, LocatedError};
use crate::span::Span;
use crate::token::Token;
use crate::value::{Closure, Env, Value, type_name};

// イベントループ内で継続を呼ぶときの便宜 span（位置の意味は薄い）。
const NOSPAN: Span = Span { start: 0, end: 0 };

// ===== 評価（Evaluator）=====

// AwaError（種類）に span を添えて LocatedError にする小ヘルパ。
// 実行時エラーを「その式の位置」で包むのに使う。
fn at<T>(r: Result<T, AwaError>, span: Span) -> Result<T, LocatedError> {
    r.map_err(|e| LocatedError::at(e, span))
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
        // 名前解決: レキシカル env → ブロック内相互再帰スコープ（内側優先）→ トップレベル GLOBALS。
        // クロージャは定義時の env スナップショットを捕獲するので後で定義された兄弟を含まない。
        // フォールバックのスコープ群で相互再帰を解決する（循環参照を作らない）。
        TExprKind::Var(name) => lookup_var(name, env)
            .ok_or_else(|| LocatedError::at(AwaError::UndefinedVariable(name.clone()), span)),
        // 単項マイナス。型検査済みなので数値のはず。
        TExprKind::Neg(operand) => match eval_expr(operand, env)? {
            Value::Int(n) => Ok(Value::Int(n.wrapping_neg())),
            Value::Long(n) => Ok(Value::Long(n.wrapping_neg())),
            Value::Float(n) => Ok(Value::Float(-n)),
            _ => unreachable!("Neg のオペランドは数値（型検査済み）"),
        },
        // ~x はビット反転（整数のみ）。
        TExprKind::BitNot(operand) => match eval_expr(operand, env)? {
            Value::Int(n) => Ok(Value::Int(!n)),
            Value::Long(n) => Ok(Value::Long(!n)),
            _ => unreachable!("~ のオペランドは整数（型検査済み）"),
        },
        // 論理積/和は短絡評価（右辺は必要なときだけ評価する）。
        TExprKind::BinOp(left, Token::AndAnd, right) => match eval_expr(left, env)? {
            Value::Bool(true) => eval_expr(right, env),
            Value::Bool(false) => Ok(Value::Bool(false)),
            _ => unreachable!("&& の左辺は Bool（型検査済み）"),
        },
        TExprKind::BinOp(left, Token::OrOr, right) => match eval_expr(left, env)? {
            Value::Bool(true) => Ok(Value::Bool(true)),
            Value::Bool(false) => eval_expr(right, env),
            _ => unreachable!("|| の左辺は Bool（型検査済み）"),
        },
        TExprKind::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            at(eval_binop(l, op, r), span)
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
        // 関数呼び出し。対象と引数を評価し、call_value へ振り分ける（関数/構築子/組込）。
        TExprKind::Call(callee, args) => {
            let callee_val = eval_expr(callee, env)?;
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr(arg, env)?);
            }
            call_value(callee_val, arg_values, span)
        }
        // ブロックは新しいスコープ。親 env をコピーして始め、抜けると破棄する
        // （ブロック内の let は外に漏れない）。値は最後の文が式ならその値、
        // そうでなければ（空 or 末尾が let）Unit。
        TExprKind::Block(stmts) => eval_block_scoped(stmts, env),
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
        // 配列リテラル [e1, ...]: 各要素を評価して不変配列にまとめる。
        TExprKind::ArrayLit(elems) => {
            let mut vals = Vec::with_capacity(elems.len());
            for e in elems {
                vals.push(eval_expr(e, env)?);
            }
            Ok(Value::Array(Rc::new(vals)))
        }
        // タプルリテラル (e1, ...): 各要素を評価して組にまとめる。
        TExprKind::TupleLit(elems) => {
            let mut vals = Vec::with_capacity(elems.len());
            for e in elems {
                vals.push(eval_expr(e, env)?);
            }
            Ok(Value::Tuple(Rc::new(vals)))
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
        // 配列パターン [a, b, ...]: 長さがちょうど一致したとき各要素を照合する。
        Pattern::Array(subs) => match value {
            Value::Array(xs) if xs.len() == subs.len() => {
                let mut bindings = Vec::new();
                for (sub, x) in subs.iter().zip(xs.iter()) {
                    bindings.extend(match_pattern(sub, x)?);
                }
                Some(bindings)
            }
            _ => None,
        },
        // タプルパターン (a, b, ...): 要素数は型で一致しているので各位置を照合する。
        Pattern::Tuple(subs) => match value {
            Value::Tuple(xs) if xs.len() == subs.len() => {
                let mut bindings = Vec::new();
                for (sub, x) in subs.iter().zip(xs.iter()) {
                    bindings.extend(match_pattern(sub, x)?);
                }
                Some(bindings)
            }
            _ => None,
        },
        // or パターン: いずれかの選択肢に当たればその束縛を返す（束縛の整合性は型検査で保証済み）。
        Pattern::Or(alts) => alts.iter().find_map(|alt| match_pattern(alt, value)),
        // レコードパターンは型検査前に Ctor へ脱糖済み。
        Pattern::Record(..) => unreachable!("レコードパターンは脱糖済み"),
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
        // 剰余。整数の 0 剰余は DivisionByZero、Float は IEEE（fmod）。
        Token::Percent => match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    Err(AwaError::DivisionByZero)
                } else {
                    Ok(Value::Int(a.wrapping_rem(b)))
                }
            }
            (Value::Long(a), Value::Long(b)) => {
                if b == 0 {
                    Err(AwaError::DivisionByZero)
                } else {
                    Ok(Value::Long(a.wrapping_rem(b)))
                }
            }
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a % b)),
            _ => unreachable!(),
        },
        // ビット演算（整数のみ。シフト量は 0..63 に丸める）。
        Token::Amp => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a & b)),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a & b)),
            _ => unreachable!("& は整数（型検査済み）"),
        },
        Token::Pipe => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a | b)),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a | b)),
            _ => unreachable!("| は整数（型検査済み）"),
        },
        Token::Caret => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a ^ b)),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a ^ b)),
            _ => unreachable!("^ は整数（型検査済み）"),
        },
        Token::Shl => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_shl(b as u32))),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a.wrapping_shl(b as u32))),
            _ => unreachable!("<< は整数（型検査済み）"),
        },
        Token::Shr => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_shr(b as u32))),
            (Value::Long(a), Value::Long(b)) => Ok(Value::Long(a.wrapping_shr(b as u32))),
            _ => unreachable!(">> は整数（型検査済み）"),
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

// ブロック内に関数 let があるか（相互再帰スコープが要るか）。
fn has_block_fns(stmts: &[TStmt]) -> bool {
    stmts.iter().any(|s| matches!(s, TStmt::Let(_, e) if matches!(e.kind, TExprKind::Lambda(..))))
}

// ブロック内の関数 let をすべてクロージャ化したスコープ（相互再帰のフォールバック用）。
// Lambda 評価はクロージャ生成のみ＝本体は走らないので失敗しない。
fn block_rec_scope(stmts: &[TStmt], base: &Env) -> Env {
    let mut scope = Env::new();
    for s in stmts {
        if let TStmt::Let(name, e) = s {
            if matches!(e.kind, TExprKind::Lambda(..)) {
                if let Ok(v) = eval_expr(e, base) {
                    scope.insert(name.clone(), bind_name(v, name));
                }
            }
        }
    }
    scope
}

// ブロック本体を順に評価し、末尾の値を返す（スコープ管理は呼び出し側）。
fn eval_block_body(stmts: &[TStmt], block_env: &mut Env) -> Result<Value, LocatedError> {
    let mut value = Value::Unit;
    for stmt in stmts {
        value = match stmt {
            TStmt::Let(name, e) => {
                let v = bind_name(eval_expr(e, block_env)?, name);
                block_env.insert(name.clone(), v);
                Value::Unit
            }
            TStmt::Type(def) => {
                register_ctors(def, block_env);
                Value::Unit
            }
            TStmt::Impl(ms) => {
                register_methods(ms, block_env);
                Value::Unit
            }
            TStmt::Expr(e) => eval_expr(e, block_env)?,
        };
    }
    Ok(value)
}

// 新スコープでブロックを評価。関数 let があれば相互再帰スコープを push してから走らせ、
// 終わったら pop する（エラー時も pop）。
fn eval_block_scoped(stmts: &[TStmt], env: &Env) -> Result<Value, LocatedError> {
    let mut block_env = env.clone();
    let pushed = has_block_fns(stmts);
    if pushed {
        let rec = block_rec_scope(stmts, &block_env);
        LOCAL_RECS.with(|st| st.borrow_mut().push(rec));
    }
    let result = eval_block_body(stmts, &mut block_env);
    if pushed {
        LOCAL_RECS.with(|st| {
            st.borrow_mut().pop();
        });
    }
    result
}

// 末尾位置評価の結果。Value=確定、Tail=末尾の関数呼び出し（呼び出し元のループが処理する）。
enum Step {
    Value(Value),
    Tail(Value, Vec<Value>, Span),
}

// 式を「末尾位置」として評価する。末尾の関数呼び出しは Rust 再帰せず Step::Tail で返し、
// call_value のループが反復処理する（TCO）。末尾でない部分式は通常の eval_expr で評価する。
fn eval_tail(expr: &TExpr, env: &Env) -> Result<Step, LocatedError> {
    let span = expr.span;
    match &expr.kind {
        // 末尾の呼び出し: 対象と引数だけ評価して Tail を返す（本体には潜らない）。
        TExprKind::Call(callee, args) => {
            let callee_val = eval_expr(callee, env)?;
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr(arg, env)?);
            }
            Ok(Step::Tail(callee_val, arg_values, span))
        }
        // ブロック: 最後の式だけが末尾位置。
        // ただし相互再帰スコープが要るブロックは TCO せず、スコープ生存中に完全評価する
        //（末尾呼び出しを返すとスコープを pop した後に実行され、兄弟関数が見えなくなるため）。
        TExprKind::Block(stmts) => {
            if has_block_fns(stmts) {
                return Ok(Step::Value(eval_block_scoped(stmts, env)?));
            }
            let mut block_env = env.clone();
            let n = stmts.len();
            for (i, stmt) in stmts.iter().enumerate() {
                let last = i + 1 == n;
                match stmt {
                    TStmt::Let(name, e) => {
                        let v = bind_name(eval_expr(e, &block_env)?, name);
                        block_env.insert(name.clone(), v);
                    }
                    TStmt::Type(def) => register_ctors(def, &mut block_env),
                    TStmt::Impl(ms) => register_methods(ms, &mut block_env),
                    TStmt::Expr(e) => {
                        if last {
                            return eval_tail(e, &block_env);
                        }
                        eval_expr(e, &block_env)?;
                    }
                }
            }
            Ok(Step::Value(Value::Unit)) // 空ブロック or 末尾が let
        }
        // case: 当たった腕の本体が末尾位置。
        TExprKind::Case(subject, arms) => {
            let value = eval_expr(subject, env)?;
            for (pattern, body) in arms {
                if let Some(bindings) = match_pattern(pattern, &value) {
                    let mut arm_env = env.clone();
                    for (name, v) in bindings {
                        arm_env.insert(name, v);
                    }
                    return eval_tail(body, &arm_env);
                }
            }
            Err(LocatedError::at(AwaError::NoMatch, span))
        }
        // その他は末尾呼び出しを含まないので普通に値へ。
        _ => Ok(Step::Value(eval_expr(expr, env)?)),
    }
}

// 値を引数列に適用する（関数・構築子・組込の振り分け）。eval_expr の Call と
// イベントループの継続呼び出しの両方から使う共通経路。
fn call_value(callee: Value, arg_values: Vec<Value>, span: Span) -> Result<Value, LocatedError> {
    match callee {
        // クロージャ適用はループで回す（末尾呼び出し最適化）。本体を末尾位置で評価し、
        // 末尾が関数呼び出しなら Rust 再帰せずループを continue する＝定数スタックで反復できる。
        Value::Func(mut closure) => {
            let mut args = arg_values;
            loop {
                if args.len() != closure.params.len() {
                    return Err(LocatedError::at(
                        AwaError::ArityMismatch {
                            expected: closure.params.len(),
                            found: args.len(),
                        },
                        span,
                    ));
                }
                // 本体はキャプチャ env（レキシカルスコープ）をベースに評価する。
                let mut call_env = closure.env.clone();
                if let Some(name) = &closure.name {
                    call_env.insert(name.clone(), Value::Func(closure.clone()));
                }
                for (param, value) in closure.params.iter().zip(args) {
                    call_env.insert(param.clone(), value);
                }
                match eval_tail(&closure.body, &call_env)? {
                    Step::Value(v) => return Ok(v),
                    // 末尾呼び出し: 相手がクロージャなら積まずにループ続行、それ以外は通常適用。
                    Step::Tail(next, next_args, call_span) => match next {
                        Value::Func(c2) => {
                            closure = c2;
                            args = next_args;
                        }
                        other => return call_value(other, next_args, call_span),
                    },
                }
            }
        }
        Value::Ctor { name, arity } => {
            if arg_values.len() != arity {
                return Err(LocatedError::at(
                    AwaError::ArityMismatch {
                        expected: arity,
                        found: arg_values.len(),
                    },
                    span,
                ));
            }
            Ok(Value::Data {
                ctor: name,
                args: arg_values,
            })
        }
        Value::Native(name) => native_call(&name, arg_values, span),
        other => Err(LocatedError::at(AwaError::NotCallable(type_name(&other)), span)),
    }
}

// ===== 非同期: 論理時間イベントループ（Phase 11）=====
// ループとキューはランタイム（この thread_local）が所有する。awa は純粋のまま、
// Value::Future(id) という不透明ハンドルだけを扱う。継続は awa のクロージャで、
// ランタイムが待ち行列に保持し、Future が解決した時に呼ぶ（= flatMap）。
// 時間は論理 tick（OS 不要・完全決定的）。

// 待ち継続。クロージャを直接 Box で持つと resolve の &mut 借用と絡むので enum 化する。
enum Waiter {
    Forward(usize),       // この Future が解決したら g も同じ値で解決
    Bind(Value, usize),   // 解決値 x に対し k(x)=Future h を作り、h を g にリンク（flatMap）
    Map(Value, usize),    // 解決値 x に対し g を k(x)（普通の値）で解決（map）
}

enum FutureState {
    Resolved(Value),
    Pending(Vec<Waiter>),
}

struct Timer {
    tick: u64,
    seq: u64, // 同 tick の安定順序
    id: usize,
    value: Value,
}

struct EventLoop {
    next_id: usize,
    seq: u64,
    start: Instant, // 実時間の基点。タイマー tick はここからの経過ミリ秒。
    futures: HashMap<usize, FutureState>,
    timers: Vec<Timer>,
}

impl EventLoop {
    fn new() -> Self {
        EventLoop {
            next_id: 0,
            seq: 0,
            start: Instant::now(),
            futures: HashMap::new(),
            timers: Vec::new(),
        }
    }
    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

thread_local! {
    static LOOP: RefCell<EventLoop> = RefCell::new(EventLoop::new());
    // トップレベル束縛のスコープ。Var 解決のフォールバック先（相互再帰のため）。
    static GLOBALS: RefCell<Env> = RefCell::new(Env::new());
    // ブロック内相互再帰のスコープスタック（評価中のブロックの関数群。内側が末尾）。
    static LOCAL_RECS: RefCell<Vec<Env>> = const { RefCell::new(Vec::new()) };
}

// 名前を解決する: レキシカル env → LOCAL_RECS（内側優先）→ GLOBALS。
fn lookup_var(name: &str, env: &Env) -> Option<Value> {
    if let Some(v) = env.get(name) {
        return Some(v.clone());
    }
    let local = LOCAL_RECS.with(|st| {
        for scope in st.borrow().iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    });
    local.or_else(|| GLOBALS.with(|g| g.borrow().get(name).cloned()))
}

// プログラム実行の冒頭でループとトップレベルスコープを初期化する（状態を持ち越さない）。
fn loop_reset() {
    LOOP.with(|l| *l.borrow_mut() = EventLoop::new());
    GLOBALS.with(|g| g.borrow_mut().clear());
    LOCAL_RECS.with(|s| s.borrow_mut().clear());
}

// GLOBALS にトップレベル束縛を 1 つ登録する（eval_program が定義ごとに呼ぶ）。
fn global_define(name: &str, value: &Value) {
    GLOBALS.with(|g| g.borrow_mut().insert(name.to_string(), value.clone()));
}

fn fut_pure(v: Value) -> usize {
    LOOP.with(|l| {
        let mut l = l.borrow_mut();
        let id = l.next_id;
        l.next_id += 1;
        l.futures.insert(id, FutureState::Resolved(v));
        id
    })
}

fn fut_pending() -> usize {
    LOOP.with(|l| {
        let mut l = l.borrow_mut();
        let id = l.next_id;
        l.next_id += 1;
        l.futures.insert(id, FutureState::Pending(Vec::new()));
        id
    })
}

fn fut_delay(ms: u64, v: Value) -> usize {
    LOOP.with(|l| {
        let mut l = l.borrow_mut();
        let id = l.next_id;
        l.next_id += 1;
        l.futures.insert(id, FutureState::Pending(Vec::new()));
        let tick = l.elapsed_ms() + ms; // 実時間の締切（経過ミリ秒）
        let seq = l.seq;
        l.seq += 1;
        l.timers.push(Timer { tick, seq, id, value: v });
        id
    })
}

// 解決済みなら値（複製）を返す。
fn fut_resolved(id: usize) -> Option<Value> {
    LOOP.with(|l| match l.borrow().futures.get(&id) {
        Some(FutureState::Resolved(v)) => Some(v.clone()),
        _ => None,
    })
}

// Pending な Future に待ち継続を追加する。
fn fut_wait(id: usize, w: Waiter) {
    LOOP.with(|l| {
        if let Some(FutureState::Pending(ws)) = l.borrow_mut().futures.get_mut(&id) {
            ws.push(w);
        }
    });
}

fn expect_future(v: Value) -> Result<usize, LocatedError> {
    match v {
        Value::Future(id) => Ok(id),
        other => Err(LocatedError::from(AwaError::TypeError {
            expected: "Future",
            found: type_name(&other),
        })),
    }
}

// h が解決したら g を同じ値で解決する（flatMap の内側 Future を結果へ繋ぐ）。
fn fut_link(h: usize, g: usize) -> Result<(), LocatedError> {
    match fut_resolved(h) {
        Some(y) => fut_resolve(g, y),
        None => {
            fut_wait(h, Waiter::Forward(g));
            Ok(())
        }
    }
}

// Future を値 v で解決し、待ち継続を発火する。継続呼び出し(call_value)中は
// RefCell の借用を保持しない（再入で二重借用しないため、待ち行列を取り出してから処理）。
fn fut_resolve(id: usize, v: Value) -> Result<(), LocatedError> {
    let waiters = LOOP.with(|l| {
        let mut l = l.borrow_mut();
        match l.futures.insert(id, FutureState::Resolved(v.clone())) {
            Some(FutureState::Pending(ws)) => ws,
            _ => Vec::new(),
        }
    });
    for w in waiters {
        match w {
            Waiter::Forward(g) => fut_resolve(g, v.clone())?,
            Waiter::Bind(k, g) => {
                let h = expect_future(call_value(k, vec![v.clone()], NOSPAN)?)?;
                fut_link(h, g)?;
            }
            Waiter::Map(k, g) => {
                let y = call_value(k, vec![v.clone()], NOSPAN)?;
                fut_resolve(g, y)?;
            }
        }
    }
    Ok(())
}

// flatMap: f が解決したら k(x)=Future を作り結果 g に繋ぐ。
fn fut_flatmap(f: usize, k: Value) -> Result<usize, LocatedError> {
    let g = fut_pending();
    match fut_resolved(f) {
        Some(x) => {
            let h = expect_future(call_value(k, vec![x], NOSPAN)?)?;
            fut_link(h, g)?;
        }
        None => fut_wait(f, Waiter::Bind(k, g)),
    }
    Ok(g)
}

// map: f が解決したら g を k(x)（普通の値）で解決。
fn fut_map(f: usize, k: Value) -> Result<usize, LocatedError> {
    let g = fut_pending();
    match fut_resolved(f) {
        Some(x) => {
            let y = call_value(k, vec![x], NOSPAN)?;
            fut_resolve(g, y)?;
        }
        None => fut_wait(f, Waiter::Map(k, g)),
    }
    Ok(g)
}

// run: f が解決するまでイベントループを回す。最早タイマーを時刻順に処理し、
// キューが空で未解決ならデッドロック。
fn fut_run(f: usize, span: Span) -> Result<Value, LocatedError> {
    loop {
        if let Some(v) = fut_resolved(f) {
            return Ok(v);
        }
        let (timer, start) = LOOP.with(|l| {
            let mut l = l.borrow_mut();
            if l.timers.is_empty() {
                return (None, l.start);
            }
            // 最早 (tick, seq) のタイマーを選ぶ。
            let mut mi = 0;
            for i in 1..l.timers.len() {
                if (l.timers[i].tick, l.timers[i].seq) < (l.timers[mi].tick, l.timers[mi].seq) {
                    mi = i;
                }
            }
            (Some(l.timers.swap_remove(mi)), l.start)
        });
        match timer {
            Some(t) => {
                // 締切まで実時間で待つ（借用を手放してから sleep）。
                let elapsed = start.elapsed().as_millis() as u64;
                if t.tick > elapsed {
                    std::thread::sleep(std::time::Duration::from_millis(t.tick - elapsed));
                }
                fut_resolve(t.id, t.value)?;
            }
            None => return Err(LocatedError::at(AwaError::Deadlock, span)),
        }
    }
}

// ===== 組込関数（型変換 + 非同期）=====
// 名前で識別される native 関数。型検査済みなので引数の型は保証されている。
fn native_call(name: &str, args: Vec<Value>, span: Span) -> Result<Value, LocatedError> {
    // 非同期の組込（イベントループに触れる / 継続を呼ぶ）。
    match name {
        "pure" => {
            let v = args.into_iter().next().expect("pure の引数");
            return Ok(Value::Future(fut_pure(v)));
        }
        "delay" => {
            let mut it = args.into_iter();
            let ms = match it.next() {
                Some(Value::Int(n)) => n.max(0) as u64,
                _ => unreachable!("delay の第1引数は Int"),
            };
            let v = it.next().expect("delay の第2引数");
            return Ok(Value::Future(fut_delay(ms, v)));
        }
        "run" => {
            let f = expect_future(args.into_iter().next().expect("run の引数"))?;
            return fut_run(f, span);
        }
        "Future#flatMap" => {
            let mut it = args.into_iter();
            let f = expect_future(it.next().expect("flatMap レシーバ"))?;
            let k = it.next().expect("flatMap の関数");
            return Ok(Value::Future(fut_flatmap(f, k)?));
        }
        "Future#map" => {
            let mut it = args.into_iter();
            let f = expect_future(it.next().expect("map レシーバ"))?;
            let k = it.next().expect("map の関数");
            return Ok(Value::Future(fut_map(f, k)?));
        }
        // OS 連携: 同期 I/O・時間（副作用はランタイム＝ここに隔離）。
        "print" => {
            if let Some(Value::Str(s)) = args.into_iter().next() {
                println!("{}", s);
                let _ = std::io::stdout().flush();
            }
            return Ok(Value::Unit);
        }
        "read_line" => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| LocatedError::at(AwaError::Io(e.to_string()), span))?;
            // 末尾の改行を除く。
            let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
            return Ok(Value::Str(trimmed));
        }
        "read_file" => {
            if let Some(Value::Str(path)) = args.into_iter().next() {
                let s = std::fs::read_to_string(&path)
                    .map_err(|e| LocatedError::at(AwaError::Io(format!("{}: {}", path, e)), span))?;
                return Ok(Value::Str(s));
            }
            unreachable!("read_file の引数は String");
        }
        "write_file" => {
            let mut it = args.into_iter();
            let path = match it.next() {
                Some(Value::Str(p)) => p,
                _ => unreachable!("write_file の第1引数は String"),
            };
            let content = match it.next() {
                Some(Value::Str(c)) => c,
                _ => unreachable!("write_file の第2引数は String"),
            };
            std::fs::write(&path, content)
                .map_err(|e| LocatedError::at(AwaError::Io(format!("{}: {}", path, e)), span))?;
            return Ok(Value::Unit);
        }
        "now" => {
            // UNIX エポックからのミリ秒。
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            return Ok(Value::Int(ms));
        }
        "string_length" => {
            if let Some(Value::Str(s)) = args.into_iter().next() {
                return Ok(Value::Int(s.chars().count() as i64));
            }
            unreachable!("string_length の引数は String");
        }
        "string_byte_length" => {
            if let Some(Value::Str(s)) = args.into_iter().next() {
                return Ok(Value::Int(s.len() as i64)); // UTF-8 バイト数（Content-Length 用）
            }
            unreachable!("string_byte_length の引数は String");
        }
        // 数学関数（libm 相当）。単項 Float -> Float。
        "math_sqrt" | "math_sin" | "math_cos" | "math_tan" | "math_asin" | "math_acos"
        | "math_atan" | "math_exp" | "math_ln" | "math_log10" | "math_floor" | "math_ceil"
        | "math_round" | "math_cbrt" | "math_trunc" | "math_log2" | "math_sinh" | "math_cosh"
        | "math_tanh" => {
            let f: fn(f64) -> f64 = match name {
                "math_sqrt" => |x| x.sqrt(),
                "math_sin" => |x| x.sin(),
                "math_cos" => |x| x.cos(),
                "math_tan" => |x| x.tan(),
                "math_asin" => |x| x.asin(),
                "math_acos" => |x| x.acos(),
                "math_atan" => |x| x.atan(),
                "math_exp" => |x| x.exp(),
                "math_ln" => |x| x.ln(),
                "math_log10" => |x| x.log10(),
                "math_floor" => |x| x.floor(),
                "math_ceil" => |x| x.ceil(),
                "math_cbrt" => |x| x.cbrt(),
                "math_trunc" => |x| x.trunc(),
                "math_log2" => |x| x.log2(),
                "math_sinh" => |x| x.sinh(),
                "math_cosh" => |x| x.cosh(),
                "math_tanh" => |x| x.tanh(),
                _ => |x| x.round(),
            };
            if let Some(Value::Float(x)) = args.into_iter().next() {
                return Ok(Value::Float(f(x)));
            }
            unreachable!("math 単項の引数は Float");
        }
        "math_pow" | "math_atan2" | "math_hypot" => {
            let mut it = args.into_iter();
            if let (Some(Value::Float(a)), Some(Value::Float(b))) = (it.next(), it.next()) {
                let r = match name {
                    "math_pow" => a.powf(b),
                    "math_atan2" => a.atan2(b),
                    _ => a.hypot(b),
                };
                return Ok(Value::Float(r));
            }
            unreachable!("math 二項の引数は Float");
        }
        "string_to_int" => {
            if let Some(Value::Str(s)) = args.into_iter().next() {
                // 非数値は 0（C の atoll と揃える）。先頭の数値部だけを読む。
                let t = s.trim();
                let n = t.parse::<i64>().unwrap_or_else(|_| {
                    let end = t.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(t.len());
                    t[..end].parse::<i64>().unwrap_or(0)
                });
                return Ok(Value::Int(n));
            }
            unreachable!("string_to_int の引数は String");
        }
        // i 番目のコードポイント（範囲外は -1）。native の awa_string_char_at と揃える。
        "string_char_at" => {
            let mut it = args.into_iter();
            if let (Some(Value::Str(s)), Some(Value::Int(i))) = (it.next(), it.next()) {
                let c = if i < 0 {
                    -1
                } else {
                    s.chars().nth(i as usize).map(|c| c as i64).unwrap_or(-1)
                };
                return Ok(Value::Int(c));
            }
            unreachable!("string_char_at の引数は (String, Int)");
        }
        // コードポイント → 1 文字の String（不正値は空文字）。
        "char_to_string" => {
            if let Some(Value::Int(c)) = args.into_iter().next() {
                let s = u32::try_from(c)
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
                    .unwrap_or_default();
                return Ok(Value::Str(s));
            }
            unreachable!("char_to_string の引数は Int");
        }
        // ファイル擬似非同期: インタプリタでは同期読み込みして即解決 Future を返す
        //（native はスレッドプールで本当に非同期）。
        "read_file_async" => {
            if let Some(Value::Str(path)) = args.into_iter().next() {
                let s = std::fs::read_to_string(&path)
                    .map_err(|e| LocatedError::at(AwaError::Io(format!("{}: {}", path, e)), span))?;
                return Ok(Value::Future(fut_pure(Value::Str(s))));
            }
            unreachable!("read_file_async の引数は String");
        }
        // ===== 配列プリミティブ（不変・永続 Array(a)） =====
        // 空配列。std/array が map/filter の初期値などに使う。
        "array_empty" => {
            return Ok(Value::Array(Rc::new(Vec::new())));
        }
        // 長さ（O(1)）。
        "array_length" => {
            if let Some(Value::Array(xs)) = args.into_iter().next() {
                return Ok(Value::Int(xs.len() as i64));
            }
            unreachable!("array_length の引数は Array");
        }
        // 無検査の要素取り出し（O(1)）。範囲外は std 側で防ぐ前提（at が Option を返す）。
        "array_get" => {
            let mut it = args.into_iter();
            if let (Some(Value::Array(xs)), Some(Value::Int(i))) = (it.next(), it.next()) {
                if i >= 0 && (i as usize) < xs.len() {
                    return Ok(xs[i as usize].clone());
                }
                return Err(LocatedError::at(
                    AwaError::Io(format!("array_get: 範囲外 index {}（長さ {}）", i, xs.len())),
                    span,
                ));
            }
            unreachable!("array_get の引数は (Array, Int)");
        }
        // i 番目を v に差し替えた新しい配列を返す（永続更新・O(n) コピー）。範囲外は元の配列を返す。
        "array_set" => {
            let mut it = args.into_iter();
            if let (Some(Value::Array(xs)), Some(Value::Int(i)), Some(v)) =
                (it.next(), it.next(), it.next())
            {
                if i < 0 || (i as usize) >= xs.len() {
                    return Ok(Value::Array(xs));
                }
                let mut new = (*xs).clone();
                new[i as usize] = v;
                return Ok(Value::Array(Rc::new(new)));
            }
            unreachable!("array_set の引数は (Array, Int, a)");
        }
        // 末尾に v を加えた新しい配列を返す（永続更新・O(n) コピー）。
        "array_push" => {
            let mut it = args.into_iter();
            if let (Some(Value::Array(xs)), Some(v)) = (it.next(), it.next()) {
                let mut new = (*xs).clone();
                new.push(v);
                return Ok(Value::Array(Rc::new(new)));
            }
            unreachable!("array_push の引数は (Array, a)");
        }
        // 可変ビルダー（transient）。容量ヒント付きで空のビルダーを作る。
        "array_builder" => {
            let cap = match args.into_iter().next() {
                Some(Value::Int(n)) => n.max(0) as usize,
                _ => unreachable!("array_builder の引数は Int"),
            };
            return Ok(Value::Builder(Rc::new(std::cell::RefCell::new(Vec::with_capacity(cap)))));
        }
        // 破壊的に末尾へ追加し、同じビルダーを返す（amortized O(1)・線形使用前提）。
        "array_builder_push" => {
            let mut it = args.into_iter();
            if let (Some(Value::Builder(b)), Some(v)) = (it.next(), it.next()) {
                b.borrow_mut().push(v);
                return Ok(Value::Builder(b));
            }
            unreachable!("array_builder_push の引数は (Builder, a)");
        }
        // ビルダーを不変配列へ凍結する（ビルダーを消費）。
        "array_build" => {
            if let Some(Value::Builder(b)) = args.into_iter().next() {
                let v = b.borrow().clone();
                return Ok(Value::Array(Rc::new(v)));
            }
            unreachable!("array_build の引数は Builder");
        }
        // ソケットはネイティブの kqueue ランタイム専用。インタプリタでは未対応。
        "tcp_listen" | "sock_port" | "accept" | "sock_read" | "sock_write" | "sock_close"
        | "tcp_connect" => {
            return Err(LocatedError::at(
                AwaError::Io(format!("{} はネイティブビルド（awa build）専用です", name)),
                span,
            ));
        }
        _ => {}
    }
    // 型変換の組込（単項）。
    let arg = args.into_iter().next();
    let v = match (name, arg) {
        ("int_to_float", Some(Value::Int(n))) => Value::Float(n as f64),
        ("float_to_int", Some(Value::Float(n))) => Value::Int(n as i64),
        ("int_to_long", Some(Value::Int(n))) => Value::Long(n),
        ("long_to_int", Some(Value::Long(n))) => Value::Int(n),
        ("int_to_string", Some(Value::Int(n))) => Value::Str(n.to_string()),
        ("float_to_string", Some(Value::Float(n))) => Value::Str(n.to_string()),
        // 文字列 → 浮動小数（非数値は 0.0。先頭の数値部を読む＝C の atof に揃える）。
        ("string_to_float", Some(Value::Str(s))) => {
            let t = s.trim();
            let f = t.parse::<f64>().unwrap_or_else(|_| {
                let end = t
                    .find(|c: char| !c.is_ascii_digit() && !matches!(c, '-' | '+' | '.' | 'e' | 'E'))
                    .unwrap_or(t.len());
                t[..end].parse::<f64>().unwrap_or(0.0)
            });
            Value::Float(f)
        }
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
        // 非同期: イベントループ組込（型側は Checker::new が seed）。
        "pure",
        "delay",
        "run",
        "Future#flatMap",
        "Future#map",
        // OS 連携（同期 I/O・時間）。
        "print",
        "read_line",
        "read_file",
        "write_file",
        "now",
        "string_to_int",
        "string_length",
        "string_byte_length",
        "string_char_at",
        "char_to_string",
        "string_to_float",
        "math_sqrt",
        "math_sin",
        "math_cos",
        "math_tan",
        "math_asin",
        "math_acos",
        "math_atan",
        "math_exp",
        "math_ln",
        "math_log10",
        "math_floor",
        "math_ceil",
        "math_round",
        "math_pow",
        "math_atan2",
        "math_hypot",
        "math_cbrt",
        "math_trunc",
        "math_log2",
        "math_sinh",
        "math_cosh",
        "math_tanh",
        // OS 連携（ソケット非同期）。native ビルド専用（インタプリタは明示エラー）。
        "tcp_listen",
        "sock_port",
        "accept",
        "sock_read",
        "sock_write",
        "sock_close",
        "tcp_connect",
        "read_file_async",
        // 配列プリミティブ。std/array が高水準 API（map/filter/at→Option…）を組む土台。
        "array_empty",
        "array_length",
        "array_get",
        "array_set",
        "array_push",
        "array_builder",
        "array_builder_push",
        "array_build",
    ] {
        env.insert(name.to_string(), Value::Native(name.to_string()));
    }
    env
}

// プログラム（文の並び）を順に評価し、各文の値を順に返す。
// env は呼び出し側が所有して渡す（REPL は行をまたいで使い回す）。
pub fn eval_program(stmts: &[TStmt], env: &mut Env) -> Result<Vec<Value>, LocatedError> {
    loop_reset(); // 非同期イベントループを初期化（前回実行の状態を持ち越さない）
    // 引数なしのトップレベル関数 main があれば、それがプログラムの入口。
    let has_main = stmts.iter().any(is_entry_main);
    let mut results = Vec::new();
    for stmt in stmts {
        match stmt {
            TStmt::Let(name, expr) => {
                let value = bind_name(eval_expr(expr, env)?, name);
                env.insert(name.clone(), value.clone());
                global_define(name, &value); // 相互再帰のフォールバック用に登録
                results.push(value);
            }
            TStmt::Expr(expr) => results.push(eval_expr(expr, env)?),
            // 型宣言は構築子を登録するだけ。結果行は出さない。
            TStmt::Type(def) => register_ctors(def, env),
            // impl はメソッドを登録するだけ。結果行は出さない。
            TStmt::Impl(ms) => register_methods(ms, env),
        };
    }
    if has_main {
        // 定義をすべて束縛したあと入口 main() を呼ぶ（戻り値は表示せず、副作用が本体）。
        // 表示行は出さない（ネイティブの @main と挙動を揃える）。
        let main_fn = env
            .get("main")
            .cloned()
            .expect("main は束縛済みのはず");
        call_value(main_fn, Vec::new(), Span::new(0, 0))?;
        return Ok(Vec::new());
    }
    Ok(results)
}

// トップレベルの `let main = fn() { ... }`（引数なしの関数）か。これがあると入口になる。
fn is_entry_main(stmt: &TStmt) -> bool {
    matches!(
        stmt,
        TStmt::Let(name, te)
            if name == "main"
            && matches!(&te.kind, TExprKind::Lambda(params, _) if params.is_empty())
    )
}
