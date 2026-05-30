use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::ast::TExpr;

// ===== 値（実行時の値）=====
// 関数を第一級の値にしたので、もう Copy ではない（Func が本体と環境を抱える）。
// 代わりに Clone を使う。Func は Rc<Closure> で包み、値の複製を安価にする
// （本体や環境を毎回ディープコピーせず、参照カウントを増やすだけ）。
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Long(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Func(Rc<Closure>),
    // 組込関数（変換など）。名前で識別し、Call 時に eval が native_call へ振り分ける。
    Native(String),
    // 空の値（Gleam の Nil 相当）。末尾が式でないブロックや空ブロックの値。
    Unit,
    // n 引数の構築子そのもの（まだ適用されていない）。呼び出すと Data になる。
    // 例: Some は Ctor{name:"Some", arity:1}。ヌル構築子は直接 Data になる。
    Ctor { name: String, arity: usize },
    // 構築済みの ADT 値。構築子名と引数。例: Some(3) → Data{ctor:"Some", args:[Num(3)]}。
    Data { ctor: String, args: Vec<Value> },
    // 非同期の Future。ランタイム所有のイベントループ内のハンドル（id）への不透明参照。
    // 中身（解決値・待ち継続）はランタイムが持ち、awa からは見えない（純粋性を保つ）。
    Future(usize),
    // 不変・永続なインデックス配列 Array(a)。Rc で包んで複製を安価にし、更新系
    // （set/push）は新しい Vec を作って返す（永続データ構造）。
    Array(Rc<Vec<Value>>),
    // タプル (a, b, ...)。要素ごとに型が異なる固定長の組。
    Tuple(Rc<Vec<Value>>),
    // 可変ビルダー（transient）。std/array が O(n) 構築に使う。線形に使う前提で、
    // push は内部 Vec を破壊的に伸ばして同じ Rc を返す（共有しなければ安全）。
    Builder(Rc<std::cell::RefCell<Vec<Value>>>),
}

// クロージャ = 関数 + 定義時の環境。
//   params: 引数名のリスト
//   body:   本体の式
//   env:    定義された時点の環境のスナップショット（キャプチャ）
// これにより fn(x){ x + n } が外側の n を覚えていられる。
#[derive(Debug)]
pub struct Closure {
    pub params: Vec<String>,
    pub body: TExpr,
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
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Long(a), Value::Long(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            // ADT 値は構築子名と引数で構造比較（Cons(1, Nil) 同士など）。
            (
                Value::Data { ctor: c1, args: a1 },
                Value::Data { ctor: c2, args: a2 },
            ) => c1 == c2 && a1 == a2,
            // 配列・タプルは要素ごとに構造比較。
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            // ビルダーは可変・線形用途なので等価を定義しない。
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::Long(n) => write!(f, "{}", n),
            // Float は整数値でも小数点つきで表示（3 でなく 3.0）して Int と区別する。
            Value::Float(n) => {
                if n.fract() == 0.0 && n.is_finite() {
                    write!(f, "{:.1}", n)
                } else {
                    write!(f, "{}", n)
                }
            }
            Value::Str(s) => write!(f, "{}", s),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Func(c) => write!(f, "<fn/{}>", c.params.len()),
            Value::Native(name) => write!(f, "<native {}>", name),
            Value::Future(_) => write!(f, "<future>"),
            // 配列は [1, 2, 3] のように表示する。
            Value::Array(xs) => {
                write!(f, "[")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", x)?;
                }
                write!(f, "]")
            }
            // タプルは (1, "a") のように表示する。
            Value::Tuple(xs) => {
                write!(f, "(")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", x)?;
                }
                write!(f, ")")
            }
            Value::Builder(_) => write!(f, "<builder>"),
            Value::Unit => write!(f, "Nil"),
            Value::Ctor { name, arity } => write!(f, "<ctor {}/{}>", name, arity),
            // 構築済み値は Some(3) / None / Cons(1, Nil) のように表示。
            Value::Data { ctor, args } => {
                write!(f, "{}", ctor)?;
                if !args.is_empty() {
                    write!(f, "(")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", a)?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
        }
    }
}

// 値の種類名（型エラーメッセージ用）。
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "Int",
        Value::Long(_) => "Long",
        Value::Float(_) => "Float",
        Value::Str(_) => "String",
        Value::Bool(_) => "Bool",
        Value::Func(_) | Value::Ctor { .. } | Value::Native(_) => "Func",
        Value::Unit => "Nil",
        Value::Data { .. } => "Data",
        Value::Future(_) => "Future",
        Value::Array(_) => "Array",
        Value::Tuple(_) => "Tuple",
        Value::Builder(_) => "Builder",
    }
}

// 環境: 変数名 → 値。評価中ずっと持ち回り、変数の解決に使う表。
pub type Env = HashMap<String, Value>;
