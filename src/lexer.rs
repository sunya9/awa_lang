use crate::error::{AwaError, LocatedError};
use crate::span::{Span, Spanned};
use crate::token::{Token, ends_expr};

// ===== 字句解析（Lexer）=====
// 各トークンに span（ソース上の char 範囲）を添えて返す。失敗時は位置つきエラー。
pub fn tokenize(input: &str) -> Result<Vec<Spanned<Token>>, LocatedError> {
    let mut tokens: Vec<Spanned<Token>> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    // 丸括弧のネスト深さ。括弧の中では改行を終端にしない（複数行の関数呼び出し
    // や式の折り返しを自然に書けるようにするため）。波括弧 {} は数えない:
    // ブロック内では改行が文の区切りとして必要だから。
    let mut paren_depth: i32 = 0;

    while i < chars.len() {
        // このトークンの開始位置。span は start..（消費後の i）。
        let start = i;
        // 1〜2 文字トークンを push するヘルパ的クロージャは借用が絡むので、
        // 各アームで Spanned::new(..., Span::new(start, end)) を直接組む。
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            // 改行: 括弧の外で、直前のトークンが式を完結しうるなら終端を挿入する。
            // それ以外（演算子の直後、括弧内など）の改行はただの空白として無視。
            '\n' => {
                if paren_depth == 0 && tokens.last().map(|t| ends_expr(&t.node)).unwrap_or(false) {
                    tokens.push(Spanned::new(Token::Terminator, Span::new(start, start + 1)));
                }
                i += 1;
            }
            ';' => {
                // 明示的な ; も終端（任意に書ける。一行に複数文を書くときに便利）。
                tokens.push(Spanned::new(Token::Terminator, Span::new(start, start + 1)));
                i += 1;
            }
            // コメント or 除算。/ の次の文字で分岐する。
            '/' => match chars.get(i + 1) {
                // 行コメント //: 行末まで読み飛ばす（改行自体は残し、ASI に任せる）。
                Some('/') => {
                    i += 2;
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                // ブロックコメント /* ... */: 対応する */ まで読み飛ばす。
                Some('*') => {
                    i += 2;
                    loop {
                        if i + 1 >= chars.len() {
                            return Err(LocatedError::at(
                                AwaError::UnterminatedComment,
                                Span::new(start, chars.len()),
                            ));
                        }
                        if chars[i] == '*' && chars[i + 1] == '/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    tokens.push(Spanned::new(Token::Slash, Span::new(start, start + 1)));
                    i += 1;
                }
            },
            // 2 文字演算子は次の文字を覗いて判定する。
            // chars.get(i + 1) は範囲外でも None を返すので安全に先読みできる。
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Spanned::new(Token::EqEq, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    tokens.push(Spanned::new(Token::Equal, Span::new(start, start + 1)));
                    i += 1;
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Spanned::new(Token::NotEq, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    return Err(LocatedError::at(
                        AwaError::UnexpectedChar('!'),
                        Span::new(start, start + 1),
                    ));
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Spanned::new(Token::Le, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    tokens.push(Spanned::new(Token::Lt, Span::new(start, start + 1)));
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Spanned::new(Token::Ge, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    tokens.push(Spanned::new(Token::Gt, Span::new(start, start + 1)));
                    i += 1;
                }
            }
            '+' => {
                tokens.push(Spanned::new(Token::Plus, Span::new(start, start + 1)));
                i += 1;
            }
            '-' => {
                // -> は case の腕・関数型の矢印。それ以外は減算/単項マイナス。
                if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Spanned::new(Token::Arrow, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    tokens.push(Spanned::new(Token::Minus, Span::new(start, start + 1)));
                    i += 1;
                }
            }
            '*' => {
                tokens.push(Spanned::new(Token::Star, Span::new(start, start + 1)));
                i += 1;
            }
            '(' => {
                tokens.push(Spanned::new(Token::LParen, Span::new(start, start + 1)));
                paren_depth += 1;
                i += 1;
            }
            ')' => {
                tokens.push(Spanned::new(Token::RParen, Span::new(start, start + 1)));
                paren_depth = (paren_depth - 1).max(0);
                i += 1;
            }
            '{' => {
                tokens.push(Spanned::new(Token::LBrace, Span::new(start, start + 1)));
                i += 1;
            }
            '}' => {
                tokens.push(Spanned::new(Token::RBrace, Span::new(start, start + 1)));
                i += 1;
            }
            ',' => {
                tokens.push(Spanned::new(Token::Comma, Span::new(start, start + 1)));
                i += 1;
            }
            ':' => {
                tokens.push(Spanned::new(Token::Colon, Span::new(start, start + 1)));
                i += 1;
            }
            '|' => {
                tokens.push(Spanned::new(Token::Pipe, Span::new(start, start + 1)));
                i += 1;
            }
            '0'..='9' | '.' => {
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n: f64 = s.parse().map_err(|_| {
                    LocatedError::at(AwaError::InvalidNumber(s.clone()), Span::new(start, i))
                })?;
                tokens.push(Spanned::new(Token::Num(n), Span::new(start, i)));
            }
            // 識別子: 英字または _ で始まり、英数字または _ が続く。
            // 予約語だけは専用トークンに振り分ける（残りは変数名）。
            c if c.is_alphabetic() || c == '_' => {
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token = match word.as_str() {
                    "let" => Token::Let,
                    "if" => Token::If,
                    "else" => Token::Else,
                    "fn" => Token::Fn,
                    "type" => Token::Type,
                    "case" => Token::Case,
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    _ => Token::Ident(word),
                };
                tokens.push(Spanned::new(token, Span::new(start, i)));
            }
            _ => {
                return Err(LocatedError::at(
                    AwaError::UnexpectedChar(c),
                    Span::new(start, start + 1),
                ));
            }
        }
    }
    Ok(tokens)
}
