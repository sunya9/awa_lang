use std::collections::HashMap;
use std::fmt;
use std::io::{Write, stdout};
use std::rc::Rc;

// ===== エラー型 =====
// 言語全体で使うエラーを 1 つの enum にまとめる。
// これ自体が ADT（代数的データ型）の練習であり、Phase 3 で本格化する
// パターンマッチ／網羅性チェックの土台になる。
// ヴァリアントを増やすと、match で対応漏れがコンパイルエラーになる。
// PartialEq を derive しておくと、テストで assert_eq!(..., Err(AwaError::DivisionByZero))
// のようにエラーの種類まで含めて比較できる。
#[derive(Debug, PartialEq)]
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
    LBrace,  // {
    RBrace,  // }
    Semicolon, // ; 文の終端
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
            // 改行も含めすべての空白は読み飛ばす。文の区切りは ; で明示するので
            // 式を自由に複数行へ折り返せる（例: `1 +` 改行 `2;`）。
            ' ' | '\t' | '\r' | '\n' => i += 1,
            ';' => {
                tokens.push(Token::Semicolon);
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
    // if 条件 { 真の腕 } else { 偽の腕 }。三項とも式。
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    // 無名関数 fn(引数,...) { 本体 }。引数名のリストと本体の式。
    Lambda(Vec<String>, Box<Expr>),
    // 関数呼び出し callee(引数,...)。callee も式（変数とは限らない）。
    Call(Box<Expr>, Vec<Expr>),
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
// 関数を第一級の値にしたので、もう Copy ではない（Func が本体と環境を抱える）。
// 代わりに Clone を使う。Func は Rc<Closure> で包み、値の複製を安価にする
// （本体や環境を毎回ディープコピーせず、参照カウントを増やすだけ）。
#[derive(Debug, Clone)]
enum Value {
    Num(f64),
    Bool(bool),
    Func(Rc<Closure>),
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

// Value 同士の等価比較。Num/Bool は中身で比較。関数は等価性を定義しない
// （同一性比較は混乱のもとなので、常に false とする）。
// derive せず手書きにしたのは、Closure に PartialEq が無いため。
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
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
        }
    }
}

// 値の種類名（型エラーメッセージ用）。
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Num(_) => "Num",
        Value::Bool(_) => "Bool",
        Value::Func(_) => "Func",
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
//   unary      = '-' unary | call
//   call       = factor ('(' args? ')')*
//   args       = expr (',' expr)*
//   factor     = Num | Bool | Ident | '(' expr ')' | if | lambda
//   if         = 'if' expr '{' expr '}' 'else' '{' expr '}'
//   lambda     = 'fn' '(' params? ')' '{' expr '}'
//   params     = Ident (',' Ident)*
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

    // プログラム = 文の並び。各文は ; で終端する。
    fn parse_program(&mut self) -> Result<Vec<Stmt>, AwaError> {
        let mut stmts = Vec::new();
        while self.peek().is_some() {
            stmts.push(self.parse_stmt()?);
            // 各文の後に ; を必須とする（終端方式）。
            self.expect(Token::Semicolon)?;
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
            self.parse_call()
        }
    }

    // 呼び出し層。factor を読んだ後、続く ( ... ) を貪欲に呼び出しとして畳む。
    // f(1)(2) のような連鎖（高階関数の戻り値を即呼び出す）も自然に扱える。
    //   call = factor ('(' args? ')')*
    //   args = expr (',' expr)*
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
            // 無名関数 fn(引数, ...) { 本体 }。引数名を読み、本体を波括弧で囲む。
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
                let body = self.parse_expr()?;
                self.expect(Token::RBrace)?;
                Ok(Expr::Lambda(params, Box::new(body)))
            }
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
        // 値はもう Copy ではないので .cloned() で複製する。
        // Num/Bool は安価、Func は Rc のカウント増加で済む。
        Expr::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| AwaError::UndefinedVariable(name.clone())),
        // 単項マイナスは数値専用。Bool に書くと型エラー。
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
        // env.clone() がキャプチャの実体。ここで包んだ env を、呼び出し時に
        // 引数を足したスコープのベースとして使う。
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
            // これでクロージャ生成時には未束縛だった自分の名前を、本体から呼べる。
            if let Some(name) = &closure.name {
                call_env.insert(name.clone(), Value::Func(closure.clone()));
            }
            // 仮引数に実引数を束縛する。
            for (param, value) in closure.params.iter().zip(arg_values) {
                call_env.insert(param.clone(), value);
            }
            eval_expr(&closure.body, &call_env)
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

// プログラム（文の並び）を順に評価し、各文の値を順に返す。
// env は呼び出し側が所有して渡す（REPL は行をまたいで使い回す）。
//
// 「計算」だけを行い「表示」はしない純粋関数にしてある。こうすると
// テストは戻り値の Vec<Value> を assert するだけで済む（I/O と分離）。
// Let も束縛した値を結果に含める（REPL でその場で確認しやすい）。
fn eval_program(stmts: &[Stmt], env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let mut results = Vec::new();
    for stmt in stmts {
        let value = match stmt {
            Stmt::Let(name, expr) => {
                let value = eval_expr(expr, env)?;
                // 関数を let で束縛するときは、その名前をクロージャに刻む。
                // これで呼び出し時に「名前→自分自身」を注入でき、再帰が成立する。
                // Rc の中身は不変なので、名前を入れた新しい Closure を作り直す。
                let value = match value {
                    Value::Func(c) => Value::Func(Rc::new(Closure {
                        params: c.params.clone(),
                        body: c.body.clone(),
                        env: c.env.clone(),
                        name: Some(name.clone()),
                    })),
                    other => other,
                };
                // Value はもう Copy ではないので、env に入れる分を clone する。
                env.insert(name.clone(), value.clone());
                value
            }
            Stmt::Expr(expr) => eval_expr(expr, env)?,
        };
        results.push(value);
    }
    Ok(results)
}

// ===== パイプライン =====
// tokenize → parse → eval を ? で連結。どこで失敗しても Err が呼び出し側へ伝わる。
// env を借用で受け取るので、ファイル実行（使い捨て env）も REPL（持ち越す env）も
// この 1 つの関数で賄える。
fn run(source: &str, env: &mut Env) -> Result<Vec<Value>, AwaError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program()?;
    eval_program(&program, env)
}

// ===== ファイル実行 =====
// .awa ファイルを読んで一括実行する。エラーが出たら表示して終了する
// （REPL と違い、続行はしない）。
fn run_file(path: &str) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ファイルを読めません ({}): {}", path, e);
            std::process::exit(1);
        }
    };
    // ファイル実行では env は使い捨て（プログラムの実行中だけ生きる）。
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
// 1 行読んでは評価する。env はループの外で持つので、let で束縛した変数を
// 次の行から参照できる（セッション状態）。
//
// TODO(あなた): REPL のループ本体を実装してください。
//
// 使える道具:
//   - stdin から 1 行読む:
//         let mut line = String::new();
//         let n = stdin.read_line(&mut line)?;   // n == 0 なら EOF（Ctrl-D）
//   - 評価: run(&line, &mut env) が Result<Vec<Value>, AwaError> を返す。
//
// 実装の指針（ここが設計判断）:
//   1. ループで「プロンプト表示 → 1 行読む → 評価」を繰り返す。
//   2. read_line が 0（EOF, Ctrl-D）を返したらループを抜ける。
//   3. ★重要★ run がエラーを返しても REPL は落とさず、エラーを表示して
//      次の入力へ進む（ファイル実行とは逆の方針）。
//   4. 空行（改行だけ）は評価をスキップしてよい（お好みで）。
//
// 参考: プロンプトは print! の後に stdout().flush() しないと表示されない
// （println! と違い print! は改行が無く、行バッファのまま出ないことがある）。
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
            break; // EOF
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
    // 引数: awa <file> ならファイル実行、引数なしなら REPL。
    // args().nth(1) は最初の引数（args[0] は実行ファイル名なので飛ばす）。
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
        run(src, &mut env).map(|values| values.last().expect("少なくとも 1 文を期待").clone())
    }

    fn num(n: f64) -> Value {
        Value::Num(n)
    }

    #[test]
    fn 演算子優先順位() {
        assert_eq!(eval("1 + 2 * 3;"), Ok(num(7.0)));
        assert_eq!(eval("(1 + 2) * 3;"), Ok(num(9.0)));
        assert_eq!(eval("2 * (3 + 4) - 1;"), Ok(num(13.0)));
    }

    #[test]
    fn 変数束縛は後続の文から参照できる() {
        assert_eq!(eval("let x = 5; let y = x * 2 + 1; x + y;"), Ok(num(16.0)));
    }

    #[test]
    fn 単項マイナスは乗算より強く結合する() {
        assert_eq!(eval("-2 * 5;"), Ok(num(-10.0)));
        assert_eq!(eval("--3;"), Ok(num(3.0)));
    }

    #[test]
    fn if_は選ばれた腕だけを評価する() {
        assert_eq!(eval("let x = 0; if x == 0 { 100 } else { 1 / x };"), Ok(num(100.0)));
    }

    #[test]
    fn 比較は加減算より弱く結合する() {
        assert_eq!(eval("1 + 2 == 3;"), Ok(Value::Bool(true)));
    }

    #[test]
    fn ゼロ除算はエラー() {
        assert_eq!(eval("1 / 0;"), Err(AwaError::DivisionByZero));
    }

    #[test]
    fn 未定義変数はエラー() {
        assert_eq!(eval("q + 1;"), Err(AwaError::UndefinedVariable("q".to_string())));
    }

    #[test]
    fn 型エラー_boolに算術() {
        assert_eq!(
            eval("true + 1;"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn 型エラー_ifの条件が数値() {
        assert_eq!(
            eval("if 1 { 2 } else { 3 };"),
            Err(AwaError::TypeError { expected: "Bool", found: "Num" })
        );
    }

    #[test]
    fn 型エラー_numとboolの比較() {
        assert_eq!(
            eval("1 == true;"),
            Err(AwaError::TypeError { expected: "Num", found: "Bool" })
        );
    }

    #[test]
    fn repl_env持ち越し() {
        let mut env = Env::new();
        assert_eq!(run("let x = 10;", &mut env), Ok(vec![Value::Num(10.0)]));
        assert_eq!(run("x + 5;", &mut env), Ok(vec![Value::Num(15.0)]));
    }

    #[test]
    fn 括弧の対応が壊れた入力() {
        assert_eq!(eval("(1 + 2;"), Err(AwaError::UnexpectedToken("Semicolon".to_string())));
    }

    #[test]
    fn セミコロン無しはエラー() {
        assert_eq!(eval("1 + 2"), Err(AwaError::UnexpectedEof));
    }

    #[test]
    fn 基本的な関数呼び出し() {
        assert_eq!(eval("let add = fn(a, b) { a + b }; add(1, 2);"), Ok(num(3.0)));
    }

    #[test]
    fn クロージャは定義時の変数を捕捉する() {
        assert_eq!(
            eval("let make_adder = fn(n) { fn(x) { x + n } }; let add5 = make_adder(5); add5(10);"),
            Ok(num(15.0))
        );
    }

    #[test]
    fn 高階関数_関数を引数に渡せる() {
        assert_eq!(
            eval("let inc = fn(x) { x + 1 }; let twice = fn(f, x) { f(f(x)) }; twice(inc, 0);"),
            Ok(num(2.0))
        );
    }

    #[test]
    fn 再帰関数() {
        assert_eq!(
            eval("let fact = fn(n) { if n == 0 { 1 } else { n * fact(n - 1) } }; fact(5);"),
            Ok(num(120.0))
        );
    }

    #[test]
    fn レキシカルスコープ_呼び出し場所の変数は見えない() {
        assert_eq!(
            eval("let f = fn() { x }; let x = 99; f();"),
            Err(AwaError::UndefinedVariable("x".to_string()))
        );
    }

    #[test]
    fn 引数の個数が合わないとエラー() {
        assert_eq!(
            eval("let add = fn(a, b) { a + b }; add(1);"),
            Err(AwaError::ArityMismatch { expected: 2, found: 1 })
        );
    }

    #[test]
    fn 関数でない値の呼び出しはエラー() {
        assert_eq!(eval("let x = 5; x(1);"), Err(AwaError::NotCallable("Num")));
    }

    #[test]
    fn 複数行に折り返した式() {
        // 改行は空白扱い。式が複数行にまたがれる。
        assert_eq!(eval("1 +\n2 +\n3;"), Ok(num(6.0)));
    }
}
