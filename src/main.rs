use std::collections::HashMap;
use std::fmt;

// ===== エラー型 =====
// 言語全体で使うエラーを 1 つの enum にまとめる。
// これ自体が ADT（代数的データ型）の練習であり、Phase 3 で本格化する
// パターンマッチ／網羅性チェックの土台になる。
// ヴァリアントを増やすと、match で対応漏れがコンパイルエラーになる。
#[derive(Debug)]
enum AwaError {
    UnexpectedChar(char),      // 字句解析: 知らない文字
    InvalidNumber(String),     // 字句解析: 数値として parse できない
    UnexpectedToken(String),   // 構文解析: 想定外のトークン
    UnexpectedEof,             // 構文解析: 入力が途中で終わった
    DivisionByZero,            // 評価: 0 除算
    UndefinedVariable(String), // 評価: 未定義変数の参照
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
            AwaError::UndefinedVariable(name) => write!(f, "未定義の変数です: {}", name),
        }
    }
}

// ===== トークン =====
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Ident(String), // 識別子（変数名）
    Let,           // キーワード let
    Equal,         // =
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
    Newline, // 文の区切り
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
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                tokens.push(Token::Newline);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Equal);
                i += 1;
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
            // 識別子: 英字または _ で始まり、英数字または _ が続く。
            // 予約語 "let" だけは専用トークンに振り分ける（残りは変数名）。
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token = match word.as_str() {
                    "let" => Token::Let,
                    _ => Token::Ident(word),
                };
                tokens.push(token);
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
    Var(String), // 変数参照
    BinOp(Box<Expr>, Token, Box<Expr>),
}

// 文（statement）。プログラムは文の並び。
//   let x = expr   → 変数束縛（値は返さず env を更新する）
//   expr           → 式文（評価して結果を表示する）
#[derive(Debug)]
enum Stmt {
    Let(String, Expr),
    Expr(Expr),
}

// 環境: 変数名 → 値。評価中ずっと持ち回り、変数の解決に使う表。
type Env = HashMap<String, f64>;

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

    // 連続する改行を読み飛ばす。空行や文末の改行を吸収する。
    fn skip_newlines(&mut self) {
        while let Some(Token::Newline) = self.peek() {
            self.next();
        }
    }

    // プログラム = 文の並び。改行で区切られた文を末尾まで読む。
    fn parse_program(&mut self) -> Result<Vec<Stmt>, AwaError> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while self.peek().is_some() {
            stmts.push(self.parse_stmt()?);
            self.skip_newlines();
        }
        Ok(stmts)
    }

    // 文 = `let ident = expr`（束縛）または `expr`（式文）。
    fn parse_stmt(&mut self) -> Result<Stmt, AwaError> {
        if let Some(Token::Let) = self.peek() {
            self.next(); // 'let' を消費
            let name = match self.next() {
                Some(Token::Ident(name)) => name,
                Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
                None => return Err(AwaError::UnexpectedEof),
            };
            match self.next() {
                Some(Token::Equal) => {}
                Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
                None => return Err(AwaError::UnexpectedEof),
            }
            let value = self.parse_expr()?;
            Ok(Stmt::Let(name, value))
        } else {
            Ok(Stmt::Expr(self.parse_expr()?))
        }
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
            Some(Token::Ident(name)) => Ok(Expr::Var(name)),
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
// env を受け取り、変数参照を解決できるようになった。
fn eval_expr(expr: &Expr, env: &Env) -> Result<f64, AwaError> {
    match expr {
        Expr::Num(n) => Ok(*n),
        // TODO(あなた): 変数参照 Expr::Var(name) を実装してください。
        //
        // やること: env から name を引いて、その値を Ok で返す。
        //   - env は HashMap<String, f64>。env.get(name) は Option<&f64> を返す。
        //   - 見つかったら f64 をコピーして Ok(...)（&f64 → f64 は .copied() が便利）。
        //   - ★設計判断★ 見つからなかったらどうする?
        //       未定義変数はエラーにすべき（Gleam なら型検査で弾くが、今は実行時）。
        //       AwaError::UndefinedVariable(name.clone()) を Err で返す。
        //       ヒント: Option を Result に変えるイディオム `.ok_or(...)` が使える。
        //       例: env.get(name).copied().ok_or(...) で 1 行に書ける。
        Expr::Var(name) => env
            .get(name)
            .copied()
            .ok_or(AwaError::UndefinedVariable(name.clone())),
        Expr::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
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

// プログラム（文の並び）を順に評価する。1 つの env を共有するので、
// let で束縛した変数を後続の文から参照できる。
fn eval_program(stmts: &[Stmt]) -> Result<(), AwaError> {
    let mut env: Env = HashMap::new();
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, expr) => {
                let value = eval_expr(expr, &env)?;
                env.insert(name.clone(), value);
                println!("let {} = {}", name, value);
            }
            Stmt::Expr(expr) => {
                println!("{}", eval_expr(expr, &env)?);
            }
        }
    }
    Ok(())
}

// ===== パイプライン =====
// tokenize → parse → eval を ? で連結。どこで失敗しても Err が main まで伝わる。
// この「失敗しうる処理を縦に積む」形が Result の真価。
fn run(source: &str) -> Result<(), AwaError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program()?;
    eval_program(&program)
}

fn main() {
    let programs = [
        "let x = 5\nlet y = x * 2 + 1\nx + y\ny / x",
        "let a = 10\na / (a - 10)", // 0 除算
        "let p = 3\nq + 1",         // 未定義変数 q → UndefinedVariable
    ];
    for (i, src) in programs.iter().enumerate() {
        println!("=== program {} ===", i + 1);
        if let Err(e) = run(src) {
            println!("error: {}", e);
        }
    }
}

