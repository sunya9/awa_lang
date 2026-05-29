use crate::span::Span;
use crate::token::Token;
use crate::types::Type;

// ===== AST =====
// パーサが作る式。各ノードがソース上の位置（span）を持つので、型エラーや
// 実行時エラーを「どこで起きたか」と共に報告できる。
// Clone を derive する理由: 関数を値にすると、Closure が本体を保持し、
// クロージャ生成時に本体を複製できる必要があるため。
#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(f64),
    Bool(bool),     // 真偽値リテラル
    Var(String),    // 変数参照
    Neg(Box<Expr>), // 単項マイナス（例: -3, -(x + 1)）
    BinOp(Box<Expr>, Token, Box<Expr>),
    // if 条件 { ブロック } else { ブロック }。三項とも式。
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    // 無名関数 fn(引数: 型?, ...) -> 戻り型? { 本体 }。
    // 各引数は名前と任意の型注釈、加えて任意の戻り型注釈、そして本体の式。
    Lambda(Vec<(String, Option<TypeExpr>)>, Option<TypeExpr>, Box<Expr>),
    // 関数呼び出し callee(引数,...)。callee も式（変数とは限らない）。
    Call(Box<Expr>, Vec<Expr>),
    // ブロック { 文; ...; 式 }。新しいスコープを作り、最後の文が式ならその値、
    // そうでなければ（空 or 末尾が let なら）Unit を返す式。
    Block(Vec<Stmt>),
    // パターンマッチ case 主語 { パターン -> 本体; ... }。腕を上から試す。
    Case(Box<Expr>, Vec<(Pattern, Expr)>),
}

// ===== パターン（case の腕の左辺）=====
// 値を分解しつつ変数を束縛する。先頭大文字の名前は構築子、小文字は変数束縛。
#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard,                    // _ : 何にでも当たるが束縛しない
    Var(String),                 // 小文字名 : 何にでも当たり、その名前で束縛する
    Bool(bool),                  // リテラル true / false
    Num(f64),                    // リテラル数値
    Ctor(String, Vec<Pattern>),  // 構築子 Some(x), Cons(h, t), None（部分パターンは入れ子）
    // or パターン A | B | ...。どれかに当たれば成立。各選択肢は同じ変数を束縛する必要がある。
    Or(Vec<Pattern>),
}

// ===== 型式（ユーザーが書く型の構文表現）=====
// type 宣言のフィールド型や（将来の）型注釈で使う。意味論的な types::Type とは別物で、
// infer.rs の resolve_type_expr が型変数 ID へ解決する。
#[derive(Debug, Clone)]
pub enum TypeExpr {
    Con(String, Vec<TypeExpr>),               // Option(a), Num, List(a)
    Var(String),                              // a（小文字の型変数）
    Fun(Vec<TypeExpr>, Box<TypeExpr>),        // fn(a) -> b
}

// ADT 宣言 type Name(params) { Variant(fields) ... }。
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,            // "Option"
    pub params: Vec<String>,     // ["a"]（型パラメータ名）
    pub variants: Vec<Variant>,
}

// ===== 型付き AST（型検査の出力 = elaboration の結果）=====
// パーサが作る untyped な Expr/Stmt に対し、型推論が各ノードに解決済みの型を載せた
// もの。eval はこちらを実行する（型は実行時には使わないが、将来のコード生成や
// 表示の土台になる。「全ての式に型がついている」を構造で保証する）。
#[derive(Debug, Clone)]
pub struct TExpr {
    pub ty: Type,
    pub span: Span,
    pub kind: TExprKind,
}

#[derive(Debug, Clone)]
pub enum TExprKind {
    Num(f64),
    Bool(bool),
    Var(String),
    Neg(Box<TExpr>),
    BinOp(Box<TExpr>, Token, Box<TExpr>),
    If(Box<TExpr>, Box<TExpr>, Box<TExpr>),
    // 引数名のみ持つ（各引数の型は自身の ty=Fun(...) に入っている）。
    Lambda(Vec<String>, Box<TExpr>),
    Call(Box<TExpr>, Vec<TExpr>),
    Block(Vec<TStmt>),
    Case(Box<TExpr>, Vec<(Pattern, TExpr)>),
}

#[derive(Debug, Clone)]
pub enum TStmt {
    Let(String, TExpr),
    Expr(TExpr),
    Type(TypeDef), // 型宣言は値を持たない（評価では構築子登録のみ）
}

// 1 つの構築子（variant）。fields が空ならヌル構築子（None など）。
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,            // "Some"
    pub fields: Vec<TypeExpr>,   // [Var("a")]
}

// 文（statement）。プログラムやブロックは文の並び。
//   let x = expr   → 変数束縛（値は返さず env を更新する）
//   expr           → 式文（評価して結果を表示する）
#[derive(Debug, Clone)]
pub enum Stmt {
    // let 束縛。任意の型注釈つき（let x: T = expr）。
    Let(String, Option<TypeExpr>, Expr),
    Expr(Expr),
    // 型宣言。値は返さない（評価では構築子を環境に登録し、結果行は出さない）。
    Type(TypeDef),
}
