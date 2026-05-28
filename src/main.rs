use std::fmt;

// ===== エラー型 =====
// 言語全体で使うエラーを 1 つの enum にまとめる。
// これ自体が ADT（代数的データ型）の練習であり、Phase 3 で本格化する
// パターンマッチ／網羅性チェックの土台になる。
// ヴァリアントを増やすと、match で対応漏れがコンパイルエラーになる。
#[derive(Debug)]
enum AwaError {
    UnexpectedChar(char),    // 字句解析: 知らない文字
    InvalidNumber(String),   // 字句解析: 数値として parse できない
    UnexpectedToken(String), // 構文解析: 想定外のトークン
    UnexpectedEof,           // 構文解析: 入力が途中で終わった
    DivisionByZero,          // 評価: 0 除算
}

// Display を実装すると `{}` で人間向けに表示できる（Debug の `{:?}` は開発者向け）。
// 将来 Phase 4 の「良質なエラーメッセージ」へ育てていく入口。
impl fmt::Display for AwaError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AwaError::UnexpectedChar(c) => write!(f, "予期しない文字: {}", c),
            AwaError::InvalidNumber(s) => write!(f, "数値として解釈できません: {}", s),
            AwaError::UnexpectedToken(t) => write!(f, "予期しないトークン: {}", t),
            AwaError::UnexpectedEof => write!(f, "入力が途中で終わっています"),
            AwaError::DivisionByZero => write!(f, "0 で割ることはできません"),
        }
    }
}

// ===== トークン =====
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

// ===== 字句解析（Lexer）=====
// 戻り値が Result になった。失敗時は panic! ではなく Err を返す。
fn tokenize(input: &str) -> Result<Vec<Token>, AwaError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
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
            '/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '0'..='9' | '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                // parse は Result を返す。unwrap せず map_err で AwaError に変換し ? で伝播。
                let n: f64 = s.parse().map_err(|_| AwaError::InvalidNumber(s.clone()))?;
                tokens.push(Token::Num(n));
            }
            _ => return Err(AwaError::UnexpectedChar(c)),
        }
    }
    Ok(tokens)
}

// ===== AST =====
#[derive(Debug)]
enum Expr {
    Num(f64),
    BinOp(Box<Expr>, Token, Box<Expr>),
}

// ===== 構文解析（Parser）=====
// 再帰下降パーサ。優先順位を文法の階層で表現する。
//   expr   = term (('+' | '-') term)*
//   term   = factor (('*' | '/') factor)*
//   factor = Num | '(' expr ')'
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn parse_expr(&mut self) -> Result<Expr, AwaError> {
        let mut left = self.parse_term()?;
        while let Some(op) = self.peek() {
            match op {
                Token::Plus | Token::Minus => {
                    let op = self.next().unwrap();
                    let right = self.parse_term()?;
                    left = Expr::BinOp(Box::new(left), op, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<Expr, AwaError> {
        let mut left = self.parse_factor()?;
        while let Some(op) = self.peek() {
            match op {
                Token::Star | Token::Slash => {
                    let op = self.next().unwrap();
                    let right = self.parse_factor()?;
                    left = Expr::BinOp(Box::new(left), op, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> Result<Expr, AwaError> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::LParen) => {
                let e = self.parse_expr()?;
                match self.next() {
                    Some(Token::RParen) => Ok(e),
                    Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
                    None => Err(AwaError::UnexpectedEof),
                }
            }
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
    }
}

// ===== 評価（Evaluator）=====
// TODO(あなた): eval を Result<f64, AwaError> を返すように実装してください。
//
// ヒント:
//   1. Expr::Num(n) のときは Ok(*n) を返す。
//   2. Expr::BinOp(left, op, right) のときは、まず子を評価する。
//      子も失敗しうるので `?` でエラーを伝播する:
//          let l = eval(left)?;
//          let r = eval(right)?;
//   3. op に応じて四則演算し、結果を Ok(...) で包む。
//   4. ★設計判断★ 0 除算をどう扱うか。
//      f64 では 1.0 / 0.0 は panic せず inf（無限大）になってしまう。
//      言語として r == 0.0 のとき Err(AwaError::DivisionByZero) を返すべきか?
//      （Gleam なら型/実行時にどう守るか、を考える良い練習）
//   5. op が Plus/Minus/Star/Slash 以外に来ることは構文上ありえないので
//      `_ => unreachable!()` でよい。
fn eval(expr: &Expr) -> Result<f64, AwaError> {
    match expr {
        Expr::Num(n) => Ok(*n),
        Expr::BinOp(left, op, right) => {
            let l = eval(left)?;
            let r = eval(right)?;
            match op {
                Token::Plus => Ok(l + r),
                Token::Minus => Ok(l - r),
                Token::Star => Ok(l * r),
                Token::Slash => {
                    if r == 0.0 {
                        Err(AwaError::DivisionByZero)
                    } else {
                        Ok(l / r)
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

// ===== パイプライン =====
// tokenize → parse → eval を ? で連結。どこで失敗しても Err が main まで伝わる。
// この「失敗しうる処理を縦に積む」形が Result の真価。
fn run(input: &str) -> Result<f64, AwaError> {
    let tokens = tokenize(input)?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_expr()?;
    eval(&ast)
}

fn main() {
    let inputs = [
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "10 / 2 - 3",
        "2 * (3 + 4) - 1",
        "1 / 0", // 0 除算 → eval でエラーにする想定
        "1 + ",  // 途中で終わる → UnexpectedEof
        "1 @ 2", // 不正な文字 → UnexpectedChar
    ];
    for input in inputs {
        match run(input) {
            Ok(value) => println!("{} = {}", input, value),
            Err(e) => println!("{:<14} => エラー: {}", input, e),
        }
    }
}
