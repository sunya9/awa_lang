// ===== トークン =====
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Num(f64),
    Bool(bool),    // 真偽値リテラル（true / false）
    Ident(String), // 識別子（変数名）
    Let,           // キーワード let
    If,            // キーワード if
    Else,          // キーワード else
    Fn,            // キーワード fn
    Type,          // キーワード type（ADT 宣言）
    Case,          // キーワード case（パターンマッチ）
    Impl,          // キーワード impl（メソッド定義）
    Do,            // キーワード do（モナド糖衣）
    Comma,         // ,
    Dot,           // .（メソッド呼び出し）
    Colon,         // :（型注釈）
    Pipe,          // |（or パターン）
    PipeArrow,     // |>（パイプ演算子）
    Arrow,         // ->（case の腕・関数型）
    LArrow,        // <-（do の束縛）
    Equal,         // =
    EqEq,          // ==
    NotEq,         // !=
    Lt,            // <
    Gt,            // >
    Le,            // <=
    Ge,            // >=
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
    LBrace, // {
    RBrace, // }
    // 文の終端。明示的な ; と、改行から自動挿入されるものの両方がこれになる
    // （Scala 風: セミコロンは普段書かず、改行が区切りになる）。
    Terminator,
}

// このトークンで「式が完結しうる」か。改行を終端に変換するか判定する材料
// （Go/Scala の自動セミコロン推論）。値・閉じ括弧の直後の改行だけを区切りにする。
pub fn ends_expr(t: &Token) -> bool {
    matches!(
        t,
        Token::Num(_) | Token::Bool(_) | Token::Ident(_) | Token::RParen | Token::RBrace
    )
}
