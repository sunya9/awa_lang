use std::collections::HashMap;
use std::fmt;
use std::io::{Write, stdout};
use std::rc::Rc;

// ===== エラー型 =====
// 言語全体で使うエラーを 1 つの enum にまとめる。
// これ自体が ADT（代数的データ型）の練習であり、Phase 3 で本格化する
// パターンマッチ／網羅性チェックの土台になる。
// ヴァリアントを増やすと、match で対応漏れがコンパイルエラーになる。
#[derive(Debug, PartialEq)]
enum AwaError {
    UnexpectedChar(char),      // 字句解析: 知らない文字
    InvalidNumber(String),     // 字句解析: 数値として parse できない
    UnterminatedComment,       // 字句解析: /* が閉じられていない
    UnexpectedToken(String),   // 構文解析: 想定外のトークン
    UnexpectedEof,             // 構文解析: 入力が途中で終わった
    DivisionByZero,            // 評価: 0 除算
    UndefinedVariable(String), // 評価: 未定義変数の参照
    TypeError {
        // 評価: 型の不一致（例: Bool に算術、if の条件が Bool でない）
        expected: &'static str,
        found: &'static str,
    },
    NotCallable(&'static str), // 評価: 関数でない値を呼び出した
    ArityMismatch {
        // 評価: 引数の個数が宣言と合わない
        expected: usize,
        found: usize,
    },
}

// Display を実装すると `{}` で人間向けに表示できる（Debug の `{:?}` は開発者向け）。
// 将来 Phase 4 の「良質なエラーメッセージ」へ育てていく入口。
impl fmt::Display for AwaError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AwaError::UnexpectedChar(c) => write!(f, "予期しない文字: {}", c),
            AwaError::InvalidNumber(s) => write!(f, "数値として解釈できません: {}", s),
            AwaError::UnterminatedComment => write!(f, "コメント /* が閉じられていません"),
            AwaError::UnexpectedToken(t) => write!(f, "予期しないトークン: {}", t),
            AwaError::UnexpectedEof => write!(f, "入力が途中で終わっています"),
            AwaError::DivisionByZero => write!(f, "0 で割ることはできません"),
            AwaError::UndefinedVariable(name) => write!(f, "未定義の変数です: {}", name),
            AwaError::TypeError { expected, found } => {
                write!(f, "型エラー: {} を期待しましたが {} でした", expected, found)
            }
            AwaError::NotCallable(found) => {
                write!(f, "{} は関数ではないので呼び出せません", found)
            }
            AwaError::ArityMismatch { expected, found } => {
                write!(
                    f,
                    "引数の個数が違います: {} 個必要ですが {} 個でした",
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
    Fn,            // キーワード fn
    Comma,         // ,
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
    LBrace,    // {
    RBrace,    // }
    // 文の終端。明示的な ; と、改行から自動挿入されるものの両方がこれになる
    // （Scala 風: セミコロンは普段書かず、改行が区切りになる）。
    Terminator,
}

// このトークンで「式が完結しうる」か。改行を終端に変換するか判定する材料
// （Go/Scala の自動セミコロン推論）。値・閉じ括弧の直後の改行だけを区切りにする。
fn ends_expr(t: &Token) -> bool {
    matches!(
        t,
        Token::Num(_) | Token::Bool(_) | Token::Ident(_) | Token::RParen | Token::RBrace
    )
}

// ===== 字句解析（Lexer）=====
// 戻り値が Result になった。失敗時は panic! ではなく Err を返す。
fn tokenize(input: &str) -> Result<Vec<Token>, AwaError> {
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
                if paren_depth == 0
                    && tokens.last().map(ends_expr).unwrap_or(false)
                {
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

// ===== AST =====
// Clone を derive する理由: 関数を値にすると、Closure が本体 Expr を
// 保持する。クロージャ生成時に本体を複製できる必要があるため。
#[derive(Debug, Clone)]
enum Expr {
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
enum Stmt {
    Let(String, Expr),
    Expr(Expr),
}

// ===== 値（実行時の値）=====
// 関数を第一級の値にしたので、もう Copy ではない（Func が本体と環境を抱える）。
// 代わりに Clone を使う。Func は Rc<Closure> で包み、値の複製を安価にする
// （本体や環境を毎回ディープコピーせず、参照カウントを増やすだけ）。
#[derive(Debug, Clone)]
enum Value {
    Num(f64),
    Bool(bool),
    Func(Rc<Closure>),
    // 空の値（Gleam の Nil 相当）。末尾が式でないブロックや空ブロックの値。
    Unit,
}

// クロージャ = 関数 + 定義時の環境。
//   params: 引数名のリスト
//   body:   本体の式
//   env:    定義された時点の環境のスナップショット（キャプチャ）
// これにより fn(x){ x + n } が外側の n を覚えていられる。
#[derive(Debug)]
struct Closure {
    params: Vec<String>,
    body: Expr,
    env: Env,
    // この関数が let で束縛されたときの名前。再帰のために使う。
    // 無名関数（束縛されずに即使われる等）は None。
    name: Option<String>,
}

// Value 同士の等価比較。Num/Bool/Unit は中身で比較。関数は等価性を定義しない
// （常に false）。derive せず手書きにしたのは Closure に PartialEq が無いため。
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Num(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Func(c) => write!(f, "<fn/{}>", c.params.len()),
            Value::Unit => write!(f, "Nil"),
        }
    }
}

// 値の種類名（型エラーメッセージ用）。
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Num(_) => "Num",
        Value::Bool(_) => "Bool",
        Value::Func(_) => "Func",
        Value::Unit => "Nil",
    }
}

// 環境: 変数名 → 値。評価中ずっと持ち回り、変数の解決に使う表。
type Env = HashMap<String, Value>;

// ===== 構文解析（Parser）=====
// 再帰下降パーサ。優先順位を文法の階層で表現する（上ほど緩い＝後で結合）。
//   program    = (stmt terminator)* stmt?
//   stmt       = 'let' Ident '=' expr | expr
//   expr       = equality
//   equality   = comparison (('==' | '!=') comparison)*
//   comparison = additive  (('<' | '>' | '<=' | '>=') additive)*
//   additive   = term      (('+' | '-') term)*
//   term       = unary     (('*' | '/') unary)*
//   unary      = '-' unary | call
//   call       = factor ('(' args? ')')*
//   args       = expr (',' expr)*
//   factor     = Num | Bool | Ident | '(' expr ')' | if | lambda | block
//   if         = 'if' expr block 'else' block
//   lambda     = 'fn' '(' params? ')' block
//   block      = '{' (stmt terminator)* stmt? '}'
//   params     = Ident (',' Ident)*
// 文の区切り（terminator）は ; または改行（字句解析が自動挿入）。
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
    fn expect(&mut self, expected: Token) -> Result<(), AwaError> {
        match self.next() {
            Some(t) if t == expected => Ok(()),
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
    }

    // 連続する終端（; や改行由来）を読み飛ばす。空文・空行を吸収する。
    fn skip_terminators(&mut self) {
        while let Some(Token::Terminator) = self.peek() {
            self.next();
        }
    }

    // プログラム = 文の並び。文と文の間には終端が必要、末尾の終端は任意。
    fn parse_program(&mut self) -> Result<Vec<Stmt>, AwaError> {
        let mut stmts = Vec::new();
        self.skip_terminators();
        while self.peek().is_some() {
            stmts.push(self.parse_stmt()?);
            match self.peek() {
                Some(Token::Terminator) => self.skip_terminators(),
                None => break, // 末尾の終端は省略可
                Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            }
        }
        Ok(stmts)
    }

    // ブロック { 文; ...; 式 }。'{' は呼び出し側が消費済みとして '}' まで読む。
    // プログラムと同じ「文 + 終端」の繰り返しだが、終わりが } になる。
    fn parse_block(&mut self) -> Result<Expr, AwaError> {
        let mut stmts = Vec::new();
        self.skip_terminators();
        while self.peek() != Some(&Token::RBrace) {
            if self.peek().is_none() {
                return Err(AwaError::UnexpectedEof);
            }
            stmts.push(self.parse_stmt()?);
            match self.peek() {
                Some(Token::Terminator) => self.skip_terminators(),
                Some(Token::RBrace) => break, // } の直前の終端は省略可
                None => return Err(AwaError::UnexpectedEof),
                Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            }
        }
        self.expect(Token::RBrace)?;
        Ok(Expr::Block(stmts))
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
            self.expect(Token::Equal)?;
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
            self.parse_call()
        }
    }

    // 呼び出し層。factor を読んだ後、続く ( ... ) を貪欲に呼び出しとして畳む。
    // f(1)(2) のような連鎖（高階関数の戻り値を即呼び出す）も自然に扱える。
    fn parse_call(&mut self) -> Result<Expr, AwaError> {
        let mut expr = self.parse_factor()?;
        while let Some(Token::LParen) = self.peek() {
            self.next(); // '(' を消費
            let mut args = Vec::new();
            // 引数が無い呼び出し f() を許す。次が ')' でなければ引数列を読む。
            if self.peek() != Some(&Token::RParen) {
                loop {
                    args.push(self.parse_expr()?);
                    match self.peek() {
                        Some(Token::Comma) => {
                            self.next(); // ',' を消費して次の引数へ
                        }
                        _ => break,
                    }
                }
            }
            self.expect(Token::RParen)?;
            expr = Expr::Call(Box::new(expr), args);
        }
        Ok(expr)
    }

    fn parse_factor(&mut self) -> Result<Expr, AwaError> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::Bool(b)) => Ok(Expr::Bool(b)),
            Some(Token::Ident(name)) => Ok(Expr::Var(name)),
            // 無名関数 fn(引数, ...) { 本体ブロック }。
            Some(Token::Fn) => {
                self.expect(Token::LParen)?;
                let mut params = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    loop {
                        match self.next() {
                            Some(Token::Ident(name)) => params.push(name),
                            Some(other) => {
                                return Err(AwaError::UnexpectedToken(format!("{:?}", other)));
                            }
                            None => return Err(AwaError::UnexpectedEof),
                        }
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.next();
                            }
                            _ => break,
                        }
                    }
                }
                self.expect(Token::RParen)?;
                self.expect(Token::LBrace)?;
                let body = self.parse_block()?;
                Ok(Expr::Lambda(params, Box::new(body)))
            }
            // if 条件 { ブロック } else { ブロック }。各腕はブロック式。
            Some(Token::If) => {
                let cond = self.parse_expr()?;
                self.expect(Token::LBrace)?;
                let then_branch = self.parse_block()?;
                self.expect(Token::Else)?;
                self.expect(Token::LBrace)?;
                let else_branch = self.parse_block()?;
                Ok(Expr::If(
                    Box::new(cond),
                    Box::new(then_branch),
                    Box::new(else_branch),
                ))
            }
            // 単独のブロック式 { ... }。
            Some(Token::LBrace) => self.parse_block(),
            Some(Token::LParen) => {
                let e = self.parse_expr()?;
                self.expect(Token::RParen)?;
                Ok(e)
            }
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
    }
}

// ===== 評価（Evaluator）=====

// 値から f64 を取り出す。数値でなければ型エラー（算術・比較は数値専用）。
fn as_num(v: Value) -> Result<f64, AwaError> {
    match v {
        Value::Num(n) => Ok(n),
        other => Err(AwaError::TypeError {
            expected: "Num",
            found: type_name(&other),
        }),
    }
}

// 値から bool を取り出す。真偽値でなければ型エラー（if の条件は Bool 専用）。
fn as_bool(v: Value) -> Result<bool, AwaError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(AwaError::TypeError {
            expected: "Bool",
            found: type_name(&other),
        }),
    }
}

// 関数値に名前を刻む（再帰のため）。関数以外はそのまま返す。
// let で束縛するときに使い、呼び出し時に「名前→自分自身」を注入できるようにする。
fn bind_name(value: Value, name: &str) -> Value {
    match value {
        Value::Func(c) => Value::Func(Rc::new(Closure {
            params: c.params.clone(),
            body: c.body.clone(),
            env: c.env.clone(),
            name: Some(name.to_string()),
        })),
        other => other,
    }
}

fn eval_expr(expr: &Expr, env: &Env) -> Result<Value, AwaError> {
    match expr {
        Expr::Num(n) => Ok(Value::Num(*n)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        // 値はもう Copy ではないので .cloned() で複製する。
        // Num/Bool は安価、Func は Rc のカウント増加で済む。
        Expr::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| AwaError::UndefinedVariable(name.clone())),
        Expr::Neg(operand) => Ok(Value::Num(-as_num(eval_expr(operand, env)?)?)),
        Expr::BinOp(left, op, right) => {
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            eval_binop(l, op, r)
        }
        // if は選ばれた腕だけを評価する（短絡評価）。
        Expr::If(cond, then_branch, else_branch) => {
            if as_bool(eval_expr(cond, env)?)? {
                eval_expr(then_branch, env)
            } else {
                eval_expr(else_branch, env)
            }
        }
        // 無名関数を評価すると Closure（関数 + 現在の環境のコピー）になる。
        // env.clone() がキャプチャの実体。呼び出し時にこの env をベースに使う。
        Expr::Lambda(params, body) => Ok(Value::Func(Rc::new(Closure {
            params: params.clone(),
            body: (**body).clone(),
            env: env.clone(),
            name: None, // 束縛時に let 側で名前を付け直す（再帰のため）
        }))),
        // 関数呼び出し。
        Expr::Call(callee, args) => {
            // 1. 呼ぶ対象を評価し、関数であることを確認する。
            let callee_val = eval_expr(callee, env)?;
            let closure = match callee_val {
                Value::Func(c) => c,
                other => return Err(AwaError::NotCallable(type_name(&other))),
            };
            // 2. 引数の個数チェック。
            if args.len() != closure.params.len() {
                return Err(AwaError::ArityMismatch {
                    expected: closure.params.len(),
                    found: args.len(),
                });
            }
            // 3. 各引数を「呼び出し側の env」で評価する（ここは呼び出し側スコープ）。
            let mut arg_values = Vec::with_capacity(args.len());
            for arg in args {
                arg_values.push(eval_expr(arg, env)?);
            }
            // 本体を評価する環境を作る。ベースはキャプチャした closure.env
            // （レキシカルスコープ）で、呼び出し側 env ではない。
            let mut call_env = closure.env.clone();
            // 再帰のため、名前付き関数なら「名前→自分自身」を call_env に入れる。
            if let Some(name) = &closure.name {
                call_env.insert(name.clone(), Value::Func(closure.clone()));
            }
            // 仮引数に実引数を束縛する。
            for (param, value) in closure.params.iter().zip(arg_values) {
                call_env.insert(param.clone(), value);
            }
            eval_expr(&closure.body, &call_env)
        }
        // ブロックは新しいスコープ。親 env をコピーして始め、抜けると破棄する
        // （ブロック内の let は外に漏れない）。値は最後の文が式ならその値、
        // そうでなければ（空 or 末尾が let）Unit。
        Expr::Block(stmts) => {
            let mut block_env = env.clone();
            let mut value = Value::Unit;
            for stmt in stmts {
                value = match stmt {
                    Stmt::Let(name, e) => {
                        let v = bind_name(eval_expr(e, &block_env)?, name);
                        block_env.insert(name.clone(), v);
                        Value::Unit // let 文はブロック値に寄与しない
                    }
                    Stmt::Expr(e) => eval_expr(e, &block_env)?,
                };
            }
            Ok(value)
        }
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
        Token::EqEq => Ok(Value::Bool(values_equal(l, r)?)),
        Token::NotEq => Ok(Value::Bool(!values_equal(l, r)?)),
        _ => unreachable!(),
    }
}

fn values_equal(l: Value, r: Value) -> Result<bool, AwaError> {
    match (l, r) {
        (Value::Num(a), Value::Num(b)) => Ok(a == b),
        (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
        (Value::Unit, Value::Unit) => Ok(true),
        (l, r) => Err(AwaError::TypeError {
            expected: type_name(&l),
            found: type_name(&r),
        }),
    }
}

fn eval_program(stmts: &[Stmt], env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let mut results = Vec::new();
    for stmt in stmts {
        let value = match stmt {
            Stmt::Let(name, expr) => {
                let value = bind_name(eval_expr(expr, env)?, name);
                env.insert(name.clone(), value.clone());
                value
            }
            Stmt::Expr(expr) => eval_expr(expr, env)?,
        };
        results.push(value);
    }
    Ok(results)
}

fn run(source: &str, env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program()?;
    eval_program(&program, env)
}

// ===== ファイル実行 =====
fn run_file(path: &str) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ファイルを読めません ({}): {}", path, e);
            std::process::exit(1);
        }
    };
    let mut env: Env = HashMap::new();
    match run(&source, &mut env) {
        Ok(values) => {
            for v in values {
                println!("{}", v);
            }
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

// ===== REPL（対話実行）=====
fn repl() -> std::io::Result<()> {
    println!("awa REPL — Ctrl-D で終了");
    let mut env: Env = HashMap::new();
    let stdin = std::io::stdin();
    loop {
        print!("awa> ");
        stdout().flush()?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line);
        if n? == 0 {
            break;
        }
        let result = run(&line, &mut env);
        match result {
            Ok(values) => {
                for v in values {
                    println!("{}", v);
                }
            }
            Err(e) => eprintln!("エラー: {}", e),
        }
    }
    Ok(())
}

fn main() {
    match std::env::args().nth(1) {
        Some(path) => run_file(&path),
        None => {
            if let Err(e) = repl() {
                eprintln!("REPL エラー: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// ===== テスト =====
#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> Result<Value, AwaError> {
        let mut env = Env::new();
        run(src, &mut env).map(|values| values.last().cloned().unwrap_or(Value::Unit))
    }

    fn num(n: f64) -> Value {
        Value::Num(n)
    }

    #[test]
    fn 演算子優先順位() {
        assert_eq!(eval("1 + 2 * 3"), Ok(num(7.0)));
        assert_eq!(eval("(1 + 2) * 3"), Ok(num(9.0)));
        assert_eq!(eval("2 * (3 + 4) - 1"), Ok(num(13.0)));
    }

    #[test]
    fn 変数束縛は後続の文から参照できる() {
        assert_eq!(eval("let x = 5; let y = x * 2 + 1; x + y"), Ok(num(16.0)));
    }

    #[test]
    fn 単項マイナスは乗算より強く結合する() {
        assert_eq!(eval("-2 * 5"), Ok(num(-10.0)));
        assert_eq!(eval("--3"), Ok(num(3.0)));
    }

    #[test]
    fn if_は選ばれた腕だけを評価する() {
        assert_eq!(eval("let x = 0; if x == 0 { 100 } else { 1 / x }"), Ok(num(100.0)));
    }

    #[test]
    fn 比較は加減算より弱く結合する() {
        assert_eq!(eval("1 + 2 == 3"), Ok(Value::Bool(true)));
    }

    #[test]
    fn ゼロ除算はエラー() {
        assert_eq!(eval("1 / 0"), Err(AwaError::DivisionByZero));
    }

    #[test]
    fn 未定義変数はエラー() {
        assert_eq!(eval("q + 1"), Err(AwaError::UndefinedVariable("q".to_string())));
    }

    #[test]
    fn 型エラー_boolに算術() {
        assert_eq!(
            eval("true + 1"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn 型エラー_ifの条件が数値() {
        assert_eq!(
            eval("if 1 { 2 } else { 3 }"),
            Err(AwaError::TypeError { expected: "Bool", found: "Num" })
        );
    }

    #[test]
    fn 型エラー_numとboolの比較() {
        assert_eq!(
            eval("1 == true"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn repl_env持ち越し() {
        let mut env = Env::new();
        assert_eq!(run("let x = 10", &mut env), Ok(vec![Value::Num(10.0)]));
        assert_eq!(run("x + 5", &mut env), Ok(vec![Value::Num(15.0)]));
    }

    #[test]
    fn 括弧の対応が壊れた入力() {
        // (1 + 2 ; → ) を期待した位置で終端（;）が来る
        assert_eq!(eval("(1 + 2;"), Err(AwaError::UnexpectedToken("Terminator".to_string())));
    }

    #[test]
    fn 基本的な関数呼び出し() {
        assert_eq!(eval("let add = fn(a, b) { a + b }; add(1, 2)"), Ok(num(3.0)));
    }

    #[test]
    fn クロージャは定義時の変数を捕捉する() {
        assert_eq!(
            eval("let make_adder = fn(n) { fn(x) { x + n } }; let add5 = make_adder(5); add5(10)"),
            Ok(num(15.0))
        );
    }

    #[test]
    fn 高階関数_関数を引数に渡せる() {
        assert_eq!(
            eval("let inc = fn(x) { x + 1 }; let twice = fn(f, x) { f(f(x)) }; twice(inc, 0)"),
            Ok(num(2.0))
        );
    }

    #[test]
    fn 再帰関数() {
        assert_eq!(
            eval("let fact = fn(n) { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5)"),
            Ok(num(120.0))
        );
    }

    #[test]
    fn レキシカルスコープ_呼び出し場所の変数は見えない() {
        assert_eq!(
            eval("let f = fn() { x }; let x = 99; f()"),
            Err(AwaError::UndefinedVariable("x".to_string()))
        );
    }

    #[test]
    fn 引数の個数が合わないとエラー() {
        assert_eq!(
            eval("let add = fn(a, b) { a + b }; add(1)"),
            Err(AwaError::ArityMismatch { expected: 2, found: 1 })
        );
    }

    #[test]
    fn 関数でない値の呼び出しはエラー() {
        assert_eq!(eval("let x = 5; x(1)"), Err(AwaError::NotCallable("Num")));
    }

    // ===== Scala 風の文法（ASI・ブロック・Unit・コメント）=====

    #[test]
    fn セミコロン無しでも単文は評価できる() {
        // 終端も改行もない単一の式。末尾の終端は省略可。
        assert_eq!(eval("1 + 2"), Ok(num(3.0)));
    }

    #[test]
    fn 改行が文の区切りになる() {
        // セミコロン無しでも改行で 2 文に分かれる（自動セミコロン推論）。
        let mut env = Env::new();
        assert_eq!(
            run("let x = 5\nx + 1", &mut env),
            Ok(vec![Value::Num(5.0), Value::Num(6.0)])
        );
    }

    #[test]
    fn 演算子の直後の改行は継続扱い() {
        // 行末が + なので改行は区切りにならず、式が次行へ続く。
        assert_eq!(eval("1 +\n2 +\n3"), Ok(num(6.0)));
    }

    #[test]
    fn 区切り無しで式が連続するとエラー() {
        // 1 行に区切り無しで 2 式 → 2 つ目でエラー。
        assert_eq!(eval("1 2"), Err(AwaError::UnexpectedToken("Num(2.0)".to_string())));
    }

    #[test]
    fn ブロックの値は末尾の式() {
        assert_eq!(eval("let v = { let a = 2; a * 3 }; v"), Ok(num(6.0)));
    }

    #[test]
    fn ブロックは新しいスコープを作る() {
        // ブロック内の let は外に漏れない。最後の a は外側の 1。
        assert_eq!(eval("let a = 1; { let a = 99; a }; a"), Ok(num(1.0)));
    }

    #[test]
    fn 末尾がletのブロックはunit() {
        assert_eq!(eval("{ let a = 1 }"), Ok(Value::Unit));
    }

    #[test]
    fn 空ブロックはunit() {
        assert_eq!(eval("{}"), Ok(Value::Unit));
    }

    #[test]
    fn 関数本体に複数文ブロック() {
        assert_eq!(
            eval("let f = fn(x) { let y = x * 2; y + 1 }; f(10)"),
            Ok(num(21.0))
        );
    }

    #[test]
    fn 行コメントは無視される() {
        assert_eq!(eval("1 + 2 // これは無視される\n"), Ok(num(3.0)));
    }

    #[test]
    fn ブロックコメントは無視される() {
        assert_eq!(eval("1 + /* 途中の */ 2"), Ok(num(3.0)));
    }

    #[test]
    fn 閉じないブロックコメントはエラー() {
        assert_eq!(eval("1 + /* 閉じない"), Err(AwaError::UnterminatedComment));
    }

    #[test]
    fn 括弧内は改行を無視して複数行に書ける() {
        assert_eq!(eval("let add = fn(a, b) { a + b }; add(\n1,\n2\n)"), Ok(num(3.0)));
    }
}
