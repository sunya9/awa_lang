use crate::token::Token;

// ===== AST =====
// Clone を derive する理由: 関数を値にすると、Closure が本体 Expr を
// 保持する。クロージャ生成時に本体を複製できる必要があるため。
#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Bool(bool),     // 真偽値リテラル
    Var(String),    // 変数参照
    Neg(Box<Expr>), // 単項マイナス（例: -3, -(x + 1)）
    BinOp(Box<Expr>, Token, Box<Expr>),
    // if 条件 { ブロック } else { ブロック }。三項とも式。
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    // 無名関数 fn(引数,...) { 本体ブロック }。引数名のリストと本体の式。
    Lambda(Vec<String>, Box<Expr>),
    // 関数呼び出し callee(引数,...)。callee も式（変数とは限らない）。
    Call(Box<Expr>, Vec<Expr>),
    // ブロック { 文; ...; 式 }。新しいスコープを作り、最後の文が式ならその値、
    // そうでなければ（空 or 末尾が let なら）Unit を返す式。
    Block(Vec<Stmt>),
}

// 文（statement）。プログラムやブロックは文の並び。
//   let x = expr   → 変数束縛（値は返さず env を更新する）
//   expr           → 式文（評価して結果を表示する）
#[derive(Debug, Clone)]
pub enum Stmt {
    Let(String, Expr),
    Expr(Expr),
}
