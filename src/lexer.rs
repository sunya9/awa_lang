use crate::error::AwaError;
use crate::token::{Token, ends_expr};

// ===== 字句解析（Lexer）=====
// 戻り値が Result になった。失敗時は panic! ではなく Err を返す。
pub fn tokenize(input: &str) -> Result<Vec<Token>, AwaError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    // 丸括弧のネスト深さ。括弧の中では改行を終端にしない（複数行の関数呼び出し
    // や式の折り返しを自然に書けるようにするため）。波括弧 {} は数えない:
    // ブロック内では改行が文の区切りとして必要だから。
    let mut paren_depth: i32 = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            // 改行: 括弧の外で、直前のトークンが式を完結しうるなら終端を挿入する。
            // それ以外（演算子の直後、括弧内など）の改行はただの空白として無視。
            '\n' => {
                if paren_depth == 0 && tokens.last().map(ends_expr).unwrap_or(false) {
                    tokens.push(Token::Terminator);
                }
                i += 1;
            }
            ';' => {
                // 明示的な ; も終端（任意に書ける。一行に複数文を書くときに便利）。
                tokens.push(Token::Terminator);
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
                            return Err(AwaError::UnterminatedComment);
                        }
                        if chars[i] == '*' && chars[i + 1] == '/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    tokens.push(Token::Slash);
                    i += 1;
                }
            },
            // 2 文字演算子は次の文字を覗いて判定する。
            // chars.get(i + 1) は範囲外でも None を返すので安全に先読みできる。
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::EqEq);
                    i += 2;
                } else {
                    tokens.push(Token::Equal);
                    i += 1;
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::NotEq);
                    i += 2;
                } else {
                    return Err(AwaError::UnexpectedChar('!'));
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Le);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Ge);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            '+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                paren_depth += 1;
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                paren_depth = (paren_depth - 1).max(0);
                i += 1;
            }
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '0'..='9' | '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n: f64 = s.parse().map_err(|_| AwaError::InvalidNumber(s.clone()))?;
                tokens.push(Token::Num(n));
            }
            // 識別子: 英字または _ で始まり、英数字または _ が続く。
            // 予約語だけは専用トークンに振り分ける（残りは変数名）。
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token = match word.as_str() {
                    "let" => Token::Let,
                    "if" => Token::If,
                    "else" => Token::Else,
                    "fn" => Token::Fn,
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    _ => Token::Ident(word),
                };
                tokens.push(token);
            }
            _ => return Err(AwaError::UnexpectedChar(c)),
        }
    }
    Ok(tokens)
}
