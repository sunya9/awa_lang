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
    Int(i64),       // 整数リテラル
    Long(i64),      // Long リテラル
    Float(f64),     // 浮動小数リテラル
    Str(String),    // 文字列リテラル
    Bool(bool),     // 真偽値リテラル
    Var(String),    // 変数参照
    Neg(Box<Expr>), // 単項マイナス（例: -3, -(x + 1)）
    BinOp(Box<Expr>, Token, Box<Expr>),
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
    // メソッド呼び出し receiver.method(引数,...)。チェッカがレシーバの具体型で
    // 解決し、型付き AST では通常の Call へ lower される（型付き側に MethodCall は無い）。
    MethodCall(Box<Expr>, String, Vec<Expr>),
    // 修飾アクセス receiver.member（括弧なし）。モジュール名前解決でのみ意味を持ち、
    // resolve が Var("mod.member") へ潰す（型付き AST には現れない）。単一ファイルでは無効。
    Field(Box<Expr>, String),
    // 配列リテラル [e1, e2, ...]。不変・永続なインデックス配列 Array(a) を構築する。
    // 連結リスト（list.Cons）とは別物。空 [] も含む。
    ArrayLit(Vec<Expr>),
    // タプルリテラル (e1, e2, ...)。要素ごとに型が異なる固定長の組 Tuple(t1, ..., tn)。
    // 要素は 2 個以上（(e) は単なるグループ化）。
    TupleLit(Vec<Expr>),
}

// ===== パターン（case の腕の左辺）=====
// 値を分解しつつ変数を束縛する。先頭大文字の名前は構築子、小文字は変数束縛。
#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard,                    // _ : 何にでも当たるが束縛しない
    Var(String),                 // 小文字名 : 何にでも当たり、その名前で束縛する
    Bool(bool),                  // リテラル true / false
    Int(i64),                    // 整数リテラルパターン
    Str(String),                 // 文字列リテラルパターン
    Ctor(String, Vec<Pattern>),  // 構築子 Some(x), Cons(h, t), None（部分パターンは入れ子）
    // 配列パターン [a, b, ...]（固定長）。主語が Array(a) で長さがちょうど一致すれば成立。
    // 配列は不変だが任意長なので、配列パターンは網羅的になり得ない（常に _ 等の既定腕が要る）。
    Array(Vec<Pattern>),
    // タプルパターン (a, b, ...)。要素数は型で固定なので各位置を分解する（網羅的になり得る）。
    Tuple(Vec<Pattern>),
    // or パターン A | B | ...。どれかに当たれば成立。各選択肢は同じ変数を束縛する必要がある。
    Or(Vec<Pattern>),
}

// ===== 型式（ユーザーが書く型の構文表現）=====
// type 宣言のフィールド型や（将来の）型注釈で使う。意味論的な types::Type とは別物で、
// infer.rs の resolve_type_expr が型変数 ID へ解決する。
// 型式は span を持つ（型注釈のエラーを位置つきで報告するため）。Expr と同じ kind+span 構造。
#[derive(Debug, Clone)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExprKind {
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
    pub span: Span,              // フォーマッタのコメント再配置に使う
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
    Int(i64),
    Long(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Var(String),
    Neg(Box<TExpr>),
    BinOp(Box<TExpr>, Token, Box<TExpr>),
    // 引数名のみ持つ（各引数の型は自身の ty=Fun(...) に入っている）。
    Lambda(Vec<String>, Box<TExpr>),
    Call(Box<TExpr>, Vec<TExpr>),
    Block(Vec<TStmt>),
    Case(Box<TExpr>, Vec<(Pattern, TExpr)>),
    // 配列リテラル [e1, ...]（要素型は自身の ty=Con("Array",[elem]) に入っている）。
    ArrayLit(Vec<TExpr>),
    // タプルリテラル (e1, ...)（要素型は自身の ty=Con("Tuple",[t1,...]) に入っている）。
    TupleLit(Vec<TExpr>),
}

#[derive(Debug, Clone)]
pub enum TStmt {
    Let(String, TExpr),
    Expr(TExpr),
    Type(TypeDef), // 型宣言は値を持たない（評価では構築子登録のみ）
    // メソッド定義（elaboration 後）。eval は各メソッドをマングル名のクロージャとして登録する。
    Impl(Vec<TMethod>),
}

// 型付きメソッド。mangled は "型頭#メソッド名"（eval の env キー＝ユーザーが書けない名前）。
// codegen はメソッドを「self を第一引数に取るトップレベル関数」として扱うため、引数型・戻り型も持つ。
#[derive(Debug, Clone)]
pub struct TMethod {
    pub mangled: String,
    pub params: Vec<String>,   // 先頭が self
    pub param_tys: Vec<Type>,  // 各引数の型（params と同順、先頭が self の型）
    pub ret: Type,             // 戻り型
    pub body: TExpr,
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
    // メソッド定義 impl Name(params) { fn ... }。値は返さない。
    Impl(ImplBlock),
}

// import 文。`import app/util` → path = ["app","util"]。`import app/util as u` で別名。
#[derive(Debug, Clone)]
pub struct Import {
    pub path: Vec<String>,
    pub alias: Option<String>,
    pub span: Span, // フォーマッタのコメント再配置に使う
}

impl Import {
    // モジュールを参照するときの修飾子。別名があればそれ、無ければ末尾セグメント（"util"）。
    pub fn qualifier(&self) -> &str {
        self.alias
            .as_deref()
            .or_else(|| self.path.last().map(|s| s.as_str()))
            .unwrap_or("")
    }
}

// パースした 1 モジュール（= 1 ファイル）。先頭の import 群・本体の文・pub で公開された名前。
#[derive(Debug, Clone, Default)]
pub struct ParsedModule {
    pub imports: Vec<Import>,
    pub stmts: Vec<Stmt>,
    pub exports: std::collections::HashSet<String>,
}

// impl ブロック。ある型にメソッド群を生やす。
#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub type_name: String,       // "Option" / "Num" など（メソッドを生やす型の頭）
    pub params: Vec<String>,     // 型パラメータ（ADT なら ["a"]、組込なら空）
    pub methods: Vec<Method>,
    pub is_pub: bool,            // pub impl なら全メソッドを他モジュールへ公開
    pub span: Span,              // フォーマッタのコメント再配置に使う
}

// 1 つのメソッド。params[0] がレシーバ（self）。
#[derive(Debug, Clone)]
pub struct Method {
    pub name: String,
    pub params: Vec<(String, Option<TypeExpr>)>, // 先頭が self（型注釈は通常省略）
    pub ret: Option<TypeExpr>,
    pub body: Expr,
    pub is_pub: bool, // pub fn なら（impl が非 pub でも）このメソッドだけ公開
}
