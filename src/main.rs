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
    TypeError {
        // 評価: 型の不一致（例: Bool に算術、if の条件が Bool でない）
        expected: &'static str,
        found: &'static str,
    },
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
            AwaError::TypeError { expected, found } => {
                write!(
                    f,
                    "型エラー: {} を期待しましたが {} でした",
                    expected, found
                )
            }
        }
    }
}

// ===== トークン =====
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Bool(bool),    // 真偽値リテラル（true / false）
    Ident(String), // 識別子（変数名）
    Let,           // キーワード let
    If,            // キーワード if
    Else,          // キーワード else
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
    LBrace,  // {
    RBrace,  // }
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
                    // 今は単独の '!' に意味がない（論理否定は未実装）
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
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
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
                    "if" => Token::If,
                    "else" => Token::Else,
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

// ===== AST =====
#[derive(Debug)]
enum Expr {
    Num(f64),
    Bool(bool),     // 真偽値リテラル
    Var(String),    // 変数参照
    Neg(Box<Expr>), // 単項マイナス（例: -3, -(x + 1)）
    BinOp(Box<Expr>, Token, Box<Expr>),
    // if 条件 then 真の腕 else 偽の腕。三項とも式。
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

// 文（statement）。プログラムは文の並び。
//   let x = expr   → 変数束縛（値は返さず env を更新する）
//   expr           → 式文（評価して結果を表示する）
#[derive(Debug)]
enum Stmt {
    Let(String, Expr),
    Expr(Expr),
}

// ===== 値（実行時の値）=====
// これまで値は f64 だけだったが、比較や if のために Bool を導入。
// Gleam と同じく「数値」と「真偽値」は別の型で、混同は型エラーになる。
// Copy 可能なので env への出し入れで .copied() が使える。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Value {
    Num(f64),
    Bool(bool),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Num(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
        }
    }
}

// 値の種類名（型エラーメッセージ用）。
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Num(_) => "Num",
        Value::Bool(_) => "Bool",
    }
}

// 環境: 変数名 → 値。評価中ずっと持ち回り、変数の解決に使う表。
type Env = HashMap<String, Value>;

// ===== 構文解析（Parser）=====
// 再帰下降パーサ。優先順位を文法の階層で表現する（上ほど緩い＝後で結合）。
//   expr       = equality
//   equality   = comparison (('==' | '!=') comparison)*
//   comparison = additive  (('<' | '>' | '<=' | '>=') additive)*
//   additive   = term      (('+' | '-') term)*
//   term       = unary     (('*' | '/') unary)*
//   unary      = '-' unary | factor
//   factor     = Num | Bool | Ident | '(' expr ')' | if
//   if         = 'if' expr '{' expr '}' 'else' '{' expr '}'
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

    // 期待するトークンを 1 つ消費する。違っていればエラー。
    // if の then / else や ( ) の対応チェックに使う。
    fn expect(&mut self, expected: Token) -> Result<(), AwaError> {
        match self.next() {
            Some(t) if t == expected => Ok(()),
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
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

    // expr のエントリ。最も緩い等価比較から始める。
    fn parse_expr(&mut self) -> Result<Expr, AwaError> {
        self.parse_equality()
    }

    // == / != （最も緩い）。左結合。
    fn parse_equality(&mut self) -> Result<Expr, AwaError> {
        let mut left = self.parse_comparison()?;
        while let Some(Token::EqEq | Token::NotEq) = self.peek() {
            let op = self.next().unwrap();
            let right = self.parse_comparison()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    // < / > / <= / >= 。== より強く、+ - より弱い。左結合。
    fn parse_comparison(&mut self) -> Result<Expr, AwaError> {
        let mut left = self.parse_additive()?;
        while let Some(Token::Lt | Token::Gt | Token::Le | Token::Ge) = self.peek() {
            let op = self.next().unwrap();
            let right = self.parse_additive()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, AwaError> {
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
        let mut left = self.parse_unary()?;
        while let Some(op) = self.peek() {
            match op {
                Token::Star | Token::Slash => {
                    let op = self.next().unwrap();
                    let right = self.parse_unary()?;
                    left = Expr::BinOp(Box::new(left), op, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    // 単項マイナス。右再帰なので `--3`（= 3）のような連続適用も自然に扱える。
    // factor より高い優先順位: `-3 * 2` は `(-3) * 2`、`-2 + 1` は `(-2) + 1`。
    fn parse_unary(&mut self) -> Result<Expr, AwaError> {
        if let Some(Token::Minus) = self.peek() {
            self.next(); // '-' を消費
            let operand = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(operand)))
        } else {
            self.parse_factor()
        }
    }

    fn parse_factor(&mut self) -> Result<Expr, AwaError> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::Bool(b)) => Ok(Expr::Bool(b)),
            Some(Token::Ident(name)) => Ok(Expr::Var(name)),
            // 'if' は既に消費済み。Rust 風: if 条件 { 真の腕 } else { 偽の腕 }。
            // 波括弧で腕を囲むので、ML 風の then/else と違い境界が明確。
            // 今は各腕に式 1 つだけ（将来ここを複数文のブロックに拡張できる）。
            Some(Token::If) => {
                let cond = self.parse_expr()?;
                self.expect(Token::LBrace)?;
                let then_branch = self.parse_expr()?;
                self.expect(Token::RBrace)?;
                self.expect(Token::Else)?;
                self.expect(Token::LBrace)?;
                let else_branch = self.parse_expr()?;
                self.expect(Token::RBrace)?;
                Ok(Expr::If(
                    Box::new(cond),
                    Box::new(then_branch),
                    Box::new(else_branch),
                ))
            }
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

// 値から f64 を取り出す。Bool だったら型エラー（算術・比較は数値専用）。
fn as_num(v: Value) -> Result<f64, AwaError> {
    match v {
        Value::Num(n) => Ok(n),
        other => Err(AwaError::TypeError {
            expected: "Num",
            found: type_name(&other),
        }),
    }
}

// 値から bool を取り出す。Num だったら型エラー（if の条件は Bool 専用）。
fn as_bool(v: Value) -> Result<bool, AwaError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(AwaError::TypeError {
            expected: "Bool",
            found: type_name(&other),
        }),
    }
}

fn eval_expr(expr: &Expr, env: &Env) -> Result<Value, AwaError> {
    match expr {
        Expr::Num(n) => Ok(Value::Num(*n)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Var(name) => env
            .get(name)
            .copied()
            .ok_or_else(|| AwaError::UndefinedVariable(name.clone())),
        // 単項マイナスは数値専用。Bool に書くと型エラー。
        Expr::Neg(operand) => Ok(Value::Num(-as_num(eval_expr(operand, env)?)?)),
        Expr::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            eval_binop(l, op, r)
        }
        // TODO(あなた): if 式 Expr::If(cond, then_branch, else_branch) を実装してください。
        //
        // やること:
        //   1. cond を eval_expr で評価し、as_bool(...)? で bool を取り出す。
        //      （cond が Num だと as_bool が型エラーを返す＝Gleam 的に Bool 必須）
        //   2. その bool が true なら then_branch を、false なら else_branch を
        //      eval_expr で評価して返す。
        //   ★重要★ 選ばれた腕「だけ」を評価すること（短絡評価）。
        //      両方評価すると、通らないはずの腕の 0 除算や未定義変数で
        //      落ちてしまう。Rust の if 式はまさにこの「片方だけ」を満たす。
        Expr::If(cond, then_branch, else_branch) => match eval_expr(cond, env) {
            Ok(v) => {
                if as_bool(v)? {
                    eval_expr(then_branch, env)
                } else {
                    eval_expr(else_branch, env)
                }
            }
            Err(e) => Err(e),
        },
    }
}

// 二項演算の評価。算術は Num→Num、比較は Num→Bool、等価は同じ型同士→Bool。
fn eval_binop(l: Value, op: &Token, r: Value) -> Result<Value, AwaError> {
    match op {
        Token::Plus => Ok(Value::Num(as_num(l)? + as_num(r)?)),
        Token::Minus => Ok(Value::Num(as_num(l)? - as_num(r)?)),
        Token::Star => Ok(Value::Num(as_num(l)? * as_num(r)?)),
        Token::Slash => {
            let (a, b) = (as_num(l)?, as_num(r)?);
            if b == 0.0 {
                Err(AwaError::DivisionByZero)
            } else {
                Ok(Value::Num(a / b))
            }
        }
        Token::Lt => Ok(Value::Bool(as_num(l)? < as_num(r)?)),
        Token::Gt => Ok(Value::Bool(as_num(l)? > as_num(r)?)),
        Token::Le => Ok(Value::Bool(as_num(l)? <= as_num(r)?)),
        Token::Ge => Ok(Value::Bool(as_num(l)? >= as_num(r)?)),
        // == / != は同じ型同士のみ比較できる。Num と Bool の比較は型エラー。
        Token::EqEq => Ok(Value::Bool(values_equal(l, r)?)),
        Token::NotEq => Ok(Value::Bool(!values_equal(l, r)?)),
        _ => unreachable!(),
    }
}

fn values_equal(l: Value, r: Value) -> Result<bool, AwaError> {
    match (l, r) {
        (Value::Num(a), Value::Num(b)) => Ok(a == b),
        (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
        (l, r) => Err(AwaError::TypeError {
            expected: type_name(&l),
            found: type_name(&r),
        }),
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
        "let a = 10\na / (a - 10)",                 // 0 除算
        "let p = 3\nq + 1",                         // 未定義変数 q → UndefinedVariable
        "-3\n-2 * 5\n-(1 + 2)\nlet n = 4\n-n + 10", // 単項マイナス
        // if と比較（Rust 風の波括弧）
        "let a = 3\nlet b = 5\nif a < b { a } else { b }\nif a == b { 1 } else { 0 }",
        // 比較の優先順位: 1 + 2 == 3 は (1 + 2) == 3 → true
        "1 + 2 == 3\n2 * 3 < 5\ntrue == true\ntrue != false",
        // 短絡評価: 偽の腕の 1 / x（x=0）は評価されないはず
        "let x = 0\nif x == 0 { 100 } else { 1 / x }",
        "if 1 { 2 } else { 3 }", // 型エラー: 条件が Bool でない
        "true + 1",              // 型エラー: Bool に算術
    ];
    for (i, src) in programs.iter().enumerate() {
        println!("=== program {} ===", i + 1);
        if let Err(e) = run(src) {
            println!("error: {}", e);
        }
    }
}
