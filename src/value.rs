use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::ast::Expr;

// ===== 値（実行時の値）=====
// 関数を第一級の値にしたので、もう Copy ではない（Func が本体と環境を抱える）。
// 代わりに Clone を使う。Func は Rc<Closure> で包み、値の複製を安価にする
// （本体や環境を毎回ディープコピーせず、参照カウントを増やすだけ）。
#[derive(Debug, Clone)]
pub enum Value {
    Num(f64),
    Bool(bool),
    Func(Rc<Closure>),
    // 空の値（Gleam の Nil 相当）。末尾が式でないブロックや空ブロックの値。
    Unit,
}

// クロージャ = 関数 + 定義時の環境。
//   params: 引数名のリスト
//   body:   本体の式
//   env:    定義された時点の環境のスナップショット（キャプチャ）
// これにより fn(x){ x + n } が外側の n を覚えていられる。
#[derive(Debug)]
pub struct Closure {
    pub params: Vec<String>,
    pub body: Expr,
    pub env: Env,
    // この関数が let で束縛されたときの名前。再帰のために使う。
    // 無名関数（束縛されずに即使われる等）は None。
    pub name: Option<String>,
}

// Value 同士の等価比較。Num/Bool/Unit は中身で比較。関数は等価性を定義しない
// （常に false）。derive せず手書きにしたのは Closure に PartialEq が無いため。
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Num(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Func(c) => write!(f, "<fn/{}>", c.params.len()),
            Value::Unit => write!(f, "Nil"),
        }
    }
}

// 値の種類名（型エラーメッセージ用）。
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Num(_) => "Num",
        Value::Bool(_) => "Bool",
        Value::Func(_) => "Func",
        Value::Unit => "Nil",
    }
}

// 環境: 変数名 → 値。評価中ずっと持ち回り、変数の解決に使う表。
pub type Env = HashMap<String, Value>;
