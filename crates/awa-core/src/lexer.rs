use crate::error::{AwaError, LocatedError};
use crate::span::{Span, Spanned};
use crate::token::{Token, ends_expr};

// 文字列リテラルの構成要素。Lit=そのままの文字、Code=`${ }` 内の式（とソース上の開始 char 位置）。
enum StrPart {
    Lit(String),
    Code(String, usize),
}

// 文字列の構成要素列をトークンへ。補間が無ければ単一の Str。
// あれば `( "lit" <> __interp( <式> ) <> "lit" ... )` という連結式へ脱糖する
// （__interp はチェッカが型駆動で int_to_string 等へ展開する）。
fn emit_string(
    parts: Vec<StrPart>,
    start: usize,
    end: usize,
    tokens: &mut Vec<Spanned<Token>>,
) -> Result<(), LocatedError> {
    let sp = Span::new(start, end);
    if !parts.iter().any(|p| matches!(p, StrPart::Code(..))) {
        // 補間なし: リテラルを連結して 1 つの Str トークン。
        let mut s = String::new();
        for p in parts {
            if let StrPart::Lit(l) = p {
                s.push_str(&l);
            }
        }
        tokens.push(Spanned::new(Token::Str(s), sp));
        return Ok(());
    }
    tokens.push(Spanned::new(Token::LParen, sp));
    let mut first = true;
    for part in parts {
        if !first {
            tokens.push(Spanned::new(Token::Concat, sp));
        }
        first = false;
        match part {
            StrPart::Lit(l) => tokens.push(Spanned::new(Token::Str(l), sp)),
            StrPart::Code(code, off) => {
                tokens.push(Spanned::new(Token::Ident("__interp".to_string()), sp));
                tokens.push(Spanned::new(Token::LParen, sp));
                // 内側の式を再帰的に字句解析し、span を元ソース上の位置へずらして差し込む。
                for mut t in tokenize(&code)? {
                    t.span = Span::new(t.span.start + off, t.span.end + off);
                    tokens.push(t);
                }
                tokens.push(Spanned::new(Token::RParen, sp));
            }
        }
    }
    tokens.push(Spanned::new(Token::RParen, sp));
    Ok(())
}

// 収集したコメント（フォーマッタが再配置するために使う。通常の字句解析では捨てる）。
#[derive(Debug, Clone)]
pub struct Comment {
    pub span: Span,
    pub text: String, // "// ..." または "/* ... */" の生テキスト
    pub block: bool,  // ブロックコメントなら true
}

// ===== 字句解析（Lexer）=====
// 各トークンに span（ソース上の char 範囲）を添えて返す。失敗時は位置つきエラー。
pub fn tokenize(input: &str) -> Result<Vec<Spanned<Token>>, LocatedError> {
    let mut sink: Vec<Comment> = Vec::new();
    tokenize_collecting(input, &mut sink)
}

// トークンに加えてコメントも返す（フォーマッタ用）。
pub fn tokenize_with_comments(
    input: &str,
) -> Result<(Vec<Spanned<Token>>, Vec<Comment>), LocatedError> {
    let mut comments: Vec<Comment> = Vec::new();
    let tokens = tokenize_collecting(input, &mut comments)?;
    Ok((tokens, comments))
}

fn tokenize_collecting(
    input: &str,
    comments: &mut Vec<Comment>,
) -> Result<Vec<Spanned<Token>>, LocatedError> {
    let mut tokens: Vec<Spanned<Token>> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    // 開いているブラケットのスタック（'(' か '{'）。改行を終端にするかは「最も内側の
    // ブラケット」で決まる: '(' の中では改行を無視（複数行の式・呼び出しを自然に書ける）、
    // '{' の中では改行を文の区切りにする（ブロック・do）。これにより run(do { ... }) のように
    // 括弧の中の波括弧ブロックでも複数行の文が書ける。
    let mut brackets: Vec<char> = Vec::new();

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
                // 最も内側が '(' の時だけ改行を無視。'{' 内・トップレベルでは終端候補。
                let suppressed = matches!(brackets.last(), Some('('));
                if !suppressed && tokens.last().map(|t| ends_expr(&t.node)).unwrap_or(false) {
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
                    comments.push(Comment {
                        span: Span::new(start, i),
                        text: chars[start..i].iter().collect::<String>().trim_end().to_string(),
                        block: false,
                    });
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
                    comments.push(Comment {
                        span: Span::new(start, i),
                        text: chars[start..i].iter().collect(),
                        block: true,
                    });
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
                } else if chars.get(i + 1) == Some(&'-') {
                    // <- は do の束縛。比較の `<` に負数を続けるときは空白を空ける（x < -3）。
                    tokens.push(Spanned::new(Token::LArrow, Span::new(start, start + 2)));
                    i += 2;
                } else if chars.get(i + 1) == Some(&'>') {
                    // <> は文字列連結。
                    tokens.push(Spanned::new(Token::Concat, Span::new(start, start + 2)));
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
            '%' => {
                tokens.push(Spanned::new(Token::Percent, Span::new(start, start + 1)));
                i += 1;
            }
            '(' => {
                tokens.push(Spanned::new(Token::LParen, Span::new(start, start + 1)));
                brackets.push('(');
                i += 1;
            }
            ')' => {
                tokens.push(Spanned::new(Token::RParen, Span::new(start, start + 1)));
                if brackets.last() == Some(&'(') {
                    brackets.pop();
                }
                i += 1;
            }
            '{' => {
                tokens.push(Spanned::new(Token::LBrace, Span::new(start, start + 1)));
                brackets.push('{');
                i += 1;
            }
            '}' => {
                tokens.push(Spanned::new(Token::RBrace, Span::new(start, start + 1)));
                if brackets.last() == Some(&'{') {
                    brackets.pop();
                }
                i += 1;
            }
            // [ ] はリストリテラル。'(' と同じく中では改行を無視（複数行リテラル可）。
            '[' => {
                tokens.push(Spanned::new(Token::LBracket, Span::new(start, start + 1)));
                brackets.push('(');
                i += 1;
            }
            ']' => {
                tokens.push(Spanned::new(Token::RBracket, Span::new(start, start + 1)));
                if brackets.last() == Some(&'(') {
                    brackets.pop();
                }
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
                // |> はパイプ、|| は論理和、それ以外は or パターンの | 。
                if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Spanned::new(Token::PipeArrow, Span::new(start, start + 2)));
                    i += 2;
                } else if chars.get(i + 1) == Some(&'|') {
                    tokens.push(Spanned::new(Token::OrOr, Span::new(start, start + 2)));
                    i += 2;
                } else {
                    tokens.push(Spanned::new(Token::Pipe, Span::new(start, start + 1)));
                    i += 1;
                }
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                tokens.push(Spanned::new(Token::AndAnd, Span::new(start, start + 2)));
                i += 2;
            }
            // 数値。整数部の数字を読み、`.`+数字なら Float、続く `L` なら Long、それ以外 Int。
            // （`5.method` は Int(5)・Dot・method に分かれる＝メソッド呼び出しと両立。）
            '0'..='9' => {
                // 0x / 0o / 0b の基数付き整数リテラル（Int、末尾 L で Long、`_` 区切り可）。
                let radix = if chars.get(i) == Some(&'0') {
                    match chars.get(i + 1) {
                        Some('x') | Some('X') => Some(16),
                        Some('o') | Some('O') => Some(8),
                        Some('b') | Some('B') => Some(2),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(radix) = radix {
                    let digit_start = i + 2;
                    let mut j = digit_start;
                    while j < chars.len() && (chars[j].is_digit(radix) || chars[j] == '_') {
                        j += 1;
                    }
                    if j == digit_start {
                        let lit: String = chars[start..j].iter().collect();
                        return Err(LocatedError::at(
                            AwaError::InvalidNumber(lit),
                            Span::new(start, j),
                        ));
                    }
                    let digits: String = chars[digit_start..j].iter().filter(|c| **c != '_').collect();
                    let n = i64::from_str_radix(&digits, radix).map_err(|_| {
                        LocatedError::at(
                            AwaError::InvalidNumber(chars[start..j].iter().collect()),
                            Span::new(start, j),
                        )
                    })?;
                    if chars.get(j) == Some(&'L') {
                        j += 1;
                        tokens.push(Spanned::new(Token::Long(n), Span::new(start, j)));
                    } else {
                        tokens.push(Spanned::new(Token::Int(n), Span::new(start, j)));
                    }
                    i = j;
                    continue;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let is_float = chars.get(i) == Some(&'.')
                    && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
                if is_float {
                    i += 1; // '.'
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let s: String = chars[start..i].iter().collect();
                if is_float {
                    let n: f64 = s.parse().map_err(|_| {
                        LocatedError::at(AwaError::InvalidNumber(s.clone()), Span::new(start, i))
                    })?;
                    tokens.push(Spanned::new(Token::Float(n), Span::new(start, i)));
                } else if chars.get(i) == Some(&'L') {
                    let n: i64 = s.parse().map_err(|_| {
                        LocatedError::at(AwaError::InvalidNumber(s.clone()), Span::new(start, i))
                    })?;
                    i += 1; // 'L'
                    tokens.push(Spanned::new(Token::Long(n), Span::new(start, i)));
                } else {
                    let n: i64 = s.parse().map_err(|_| {
                        LocatedError::at(AwaError::InvalidNumber(s.clone()), Span::new(start, i))
                    })?;
                    tokens.push(Spanned::new(Token::Int(n), Span::new(start, i)));
                }
            }
            // 文字列リテラル "..."。最小のエスケープ \n \t \\ \" に対応。
            // 三連引用符 """..."""：生（エスケープ解釈なし）・改行保持の複数行文字列。
            '"' if chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') => {
                i += 3; // 開き """
                let mut parts: Vec<StrPart> = Vec::new();
                let mut lit = String::new();
                loop {
                    if chars.get(i).is_none() {
                        return Err(LocatedError::at(
                            AwaError::UnterminatedString,
                            Span::new(start, i),
                        ));
                    }
                    if chars.get(i) == Some(&'"')
                        && chars.get(i + 1) == Some(&'"')
                        && chars.get(i + 2) == Some(&'"')
                    {
                        i += 3; // 閉じ """
                        break;
                    }
                    // 生文字列でも ${ } 補間は解釈する。
                    if chars.get(i) == Some(&'$') && chars.get(i + 1) == Some(&'{') {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                        i += 2; // ${
                        let code_start = i;
                        let mut depth = 1;
                        loop {
                            match chars.get(i) {
                                None => {
                                    return Err(LocatedError::at(
                                        AwaError::UnterminatedString,
                                        Span::new(start, i),
                                    ));
                                }
                                Some('{') => {
                                    depth += 1;
                                    i += 1;
                                }
                                Some('}') => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                    i += 1;
                                }
                                Some(_) => i += 1,
                            }
                        }
                        parts.push(StrPart::Code(chars[code_start..i].iter().collect(), code_start));
                        i += 1; // 閉じ }
                        continue;
                    }
                    lit.push(chars[i]); // 中身はそのまま（改行も含めて）
                    i += 1;
                }
                parts.push(StrPart::Lit(lit));
                emit_string(parts, start, i, &mut tokens)?;
            }
            '"' => {
                i += 1; // 開き "
                let mut parts: Vec<StrPart> = Vec::new();
                let mut lit = String::new();
                loop {
                    match chars.get(i) {
                        None => {
                            return Err(LocatedError::at(
                                AwaError::UnterminatedString,
                                Span::new(start, i),
                            ));
                        }
                        Some('"') => {
                            i += 1; // 閉じ "
                            break;
                        }
                        // ${ 式 } 補間。`\$` でリテラルの $ をエスケープできる。
                        Some('$') if chars.get(i + 1) == Some(&'{') => {
                            parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                            i += 2; // ${
                            let code_start = i;
                            let mut depth = 1;
                            loop {
                                match chars.get(i) {
                                    None => {
                                        return Err(LocatedError::at(
                                            AwaError::UnterminatedString,
                                            Span::new(start, i),
                                        ));
                                    }
                                    Some('{') => {
                                        depth += 1;
                                        i += 1;
                                    }
                                    Some('}') => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                        i += 1;
                                    }
                                    Some(_) => i += 1,
                                }
                            }
                            parts.push(StrPart::Code(
                                chars[code_start..i].iter().collect(),
                                code_start,
                            ));
                            i += 1; // 閉じ }
                        }
                        Some('\\') => match chars.get(i + 1) {
                            Some('n') => {
                                lit.push('\n');
                                i += 2;
                            }
                            Some('r') => {
                                lit.push('\r');
                                i += 2;
                            }
                            Some('t') => {
                                lit.push('\t');
                                i += 2;
                            }
                            Some('\\') => {
                                lit.push('\\');
                                i += 2;
                            }
                            Some('"') => {
                                lit.push('"');
                                i += 2;
                            }
                            Some('$') => {
                                lit.push('$');
                                i += 2;
                            }
                            _ => {
                                lit.push('\\');
                                i += 1;
                            }
                        },
                        Some(&c) => {
                            lit.push(c);
                            i += 1;
                        }
                    }
                }
                parts.push(StrPart::Lit(lit));
                emit_string(parts, start, i, &mut tokens)?;
            }
            // 単独の '.' はメソッド呼び出しのドット（小数点は数値アームで処理済み）。
            '.' => {
                tokens.push(Spanned::new(Token::Dot, Span::new(start, start + 1)));
                i += 1;
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
                    "fn" => Token::Fn,
                    "type" => Token::Type,
                    "case" => Token::Case,
                    "impl" => Token::Impl,
                    "do" => Token::Do,
                    "import" => Token::Import,
                    "pub" => Token::Pub,
                    "as" => Token::As,
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
