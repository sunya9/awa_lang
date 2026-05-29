use crate::ast::{Expr, Pattern, Stmt, TypeDef, TypeExpr, Variant};
use crate::error::AwaError;
use crate::token::Token;

// 先頭大文字なら型名／構築子、小文字なら変数／型変数（Gleam と同じ規約）。
fn is_upper(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

// ===== 構文解析（Parser）=====
// 再帰下降パーサ。優先順位を文法の階層で表現する（上ほど緩い＝後で結合）。
//   program    = (stmt terminator)* stmt?
//   stmt       = 'let' Ident (':' typeexpr)? '=' expr | typedef | expr
//   typedef    = 'type' UIdent ('(' names ')')? '{' (variant terminator)* variant? '}'
//   variant    = UIdent ('(' typeexpr (',' typeexpr)* ')')?
//   typeexpr   = UIdent ('(' typeexpr,* ')')? | Ident | 'fn' '(' typeexpr,* ')' '->' typeexpr
//   expr       = equality
//   equality   = comparison (('==' | '!=') comparison)*
//   comparison = additive  (('<' | '>' | '<=' | '>=') additive)*
//   additive   = term      (('+' | '-') term)*
//   term       = unary     (('*' | '/') unary)*
//   unary      = '-' unary | call
//   call       = factor ('(' args? ')')*
//   args       = expr (',' expr)*
//   factor     = Num | Bool | Ident | '(' expr ')' | if | lambda | block | case
//   case       = 'case' expr '{' (arm terminator)* arm? '}'
//   arm        = pattern '->' expr
//   pattern    = primary ('|' primary)*
//   primary    = '_' | Ident | Num | Bool | UIdent ('(' pattern,* ')')?
//   if         = 'if' expr block 'else' block
//   lambda     = 'fn' '(' params? ')' ('->' typeexpr)? block
//   block      = '{' (stmt terminator)* stmt? '}'
//   params     = param (',' param)*
//   param      = Ident (':' typeexpr)?
// 文の区切り（terminator）は ; または改行（字句解析が自動挿入）。
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
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
    pub fn parse_program(&mut self) -> Result<Vec<Stmt>, AwaError> {
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

    // 文 = `let ident = expr`（束縛）/ `type ...`（型宣言）/ `expr`（式文）。
    fn parse_stmt(&mut self) -> Result<Stmt, AwaError> {
        match self.peek() {
            Some(Token::Let) => {
                self.next(); // 'let' を消費
                let name = match self.next() {
                    Some(Token::Ident(name)) => name,
                    Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
                    None => return Err(AwaError::UnexpectedEof),
                };
                // 任意の型注釈 let x: T = ...
                let annotation = if self.peek() == Some(&Token::Colon) {
                    self.next(); // ':'
                    Some(self.parse_type_expr()?)
                } else {
                    None
                };
                self.expect(Token::Equal)?;
                let value = self.parse_expr()?;
                Ok(Stmt::Let(name, annotation, value))
            }
            Some(Token::Type) => Ok(Stmt::Type(self.parse_type_def()?)),
            _ => Ok(Stmt::Expr(self.parse_expr()?)),
        }
    }

    // 型宣言 type Name(params) { Variant(fields) ... }。
    // params/variants は終端（改行 or ;）区切り、ブロックと同じ流儀で読む。
    fn parse_type_def(&mut self) -> Result<TypeDef, AwaError> {
        self.next(); // 'type' を消費
        let name = self.expect_upper_ident()?;
        let params = if self.peek() == Some(&Token::LParen) {
            self.parse_name_list()?
        } else {
            Vec::new()
        };
        self.expect(Token::LBrace)?;
        let mut variants = Vec::new();
        self.skip_terminators();
        while self.peek() != Some(&Token::RBrace) {
            if self.peek().is_none() {
                return Err(AwaError::UnexpectedEof);
            }
            let vname = self.expect_upper_ident()?;
            // フィールドがあれば (型式, ...)。無ければヌル構築子。
            let fields = if self.peek() == Some(&Token::LParen) {
                self.next(); // '('
                let mut fs = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    loop {
                        fs.push(self.parse_type_expr()?);
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.next();
                            }
                            _ => break,
                        }
                    }
                }
                self.expect(Token::RParen)?;
                fs
            } else {
                Vec::new()
            };
            variants.push(Variant { name: vname, fields });
            match self.peek() {
                Some(Token::Terminator) => self.skip_terminators(),
                Some(Token::RBrace) => break,
                None => return Err(AwaError::UnexpectedEof),
                Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            }
        }
        self.expect(Token::RBrace)?;
        Ok(TypeDef {
            name,
            params,
            variants,
        })
    }

    // 大文字始まりの識別子を 1 つ消費する（型名・構築子名）。
    fn expect_upper_ident(&mut self) -> Result<String, AwaError> {
        match self.next() {
            Some(Token::Ident(name)) if is_upper(&name) => Ok(name),
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
    }

    // '(' name (',' name)* ')' の小文字名リスト（型パラメータ）。'(' から読む。
    fn parse_name_list(&mut self) -> Result<Vec<String>, AwaError> {
        self.expect(Token::LParen)?;
        let mut names = Vec::new();
        if self.peek() != Some(&Token::RParen) {
            loop {
                match self.next() {
                    Some(Token::Ident(name)) => names.push(name),
                    Some(other) => return Err(AwaError::UnexpectedToken(format!("{:?}", other))),
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
        Ok(names)
    }

    // 型式: 大文字名→Con（任意の型引数）、小文字名→型変数、fn(...) -> ...→関数型。
    fn parse_type_expr(&mut self) -> Result<TypeExpr, AwaError> {
        match self.next() {
            Some(Token::Ident(name)) => {
                if is_upper(&name) {
                    // 構築子適用の型引数（任意）。
                    let args = if self.peek() == Some(&Token::LParen) {
                        self.next(); // '('
                        let mut args = Vec::new();
                        if self.peek() != Some(&Token::RParen) {
                            loop {
                                args.push(self.parse_type_expr()?);
                                match self.peek() {
                                    Some(Token::Comma) => {
                                        self.next();
                                    }
                                    _ => break,
                                }
                            }
                        }
                        self.expect(Token::RParen)?;
                        args
                    } else {
                        Vec::new()
                    };
                    Ok(TypeExpr::Con(name, args))
                } else {
                    Ok(TypeExpr::Var(name))
                }
            }
            // fn(引数型, ...) -> 戻り型。
            Some(Token::Fn) => {
                self.expect(Token::LParen)?;
                let mut params = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    loop {
                        params.push(self.parse_type_expr()?);
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.next();
                            }
                            _ => break,
                        }
                    }
                }
                self.expect(Token::RParen)?;
                self.expect(Token::Arrow)?;
                let ret = self.parse_type_expr()?;
                Ok(TypeExpr::Fun(params, Box::new(ret)))
            }
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
        }
    }

    // パターン。`|` で複数の選択肢を 1 つにまとめられる（A | B | ...）。
    // 各選択肢は parse_pattern_primary が読む。構築子の部分パターンも parse_pattern を
    // 通るので Some(A | B) のような入れ子の or も書ける。
    fn parse_pattern(&mut self) -> Result<Pattern, AwaError> {
        let first = self.parse_pattern_primary()?;
        if self.peek() != Some(&Token::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.peek() == Some(&Token::Pipe) {
            self.next(); // '|'
            alts.push(self.parse_pattern_primary()?);
        }
        Ok(Pattern::Or(alts))
    }

    // 単一パターン: _ →ワイルドカード、小文字名→束縛変数、大文字名→構築子（任意の部分パターン列）、
    // リテラル Num/Bool。構築子は入れ子の部分パターンを持てる（Some(Cons(x, _)) など）。
    fn parse_pattern_primary(&mut self) -> Result<Pattern, AwaError> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Pattern::Num(n)),
            Some(Token::Bool(b)) => Ok(Pattern::Bool(b)),
            Some(Token::Ident(name)) => {
                if name == "_" {
                    Ok(Pattern::Wildcard)
                } else if is_upper(&name) {
                    // 構築子パターン。続く ( があれば部分パターン列。
                    let subs = if self.peek() == Some(&Token::LParen) {
                        self.next(); // '('
                        let mut subs = Vec::new();
                        if self.peek() != Some(&Token::RParen) {
                            loop {
                                subs.push(self.parse_pattern()?);
                                match self.peek() {
                                    Some(Token::Comma) => {
                                        self.next();
                                    }
                                    _ => break,
                                }
                            }
                        }
                        self.expect(Token::RParen)?;
                        subs
                    } else {
                        Vec::new()
                    };
                    Ok(Pattern::Ctor(name, subs))
                } else {
                    Ok(Pattern::Var(name))
                }
            }
            Some(other) => Err(AwaError::UnexpectedToken(format!("{:?}", other))),
            None => Err(AwaError::UnexpectedEof),
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
            // 無名関数 fn(引数: 型?, ...) -> 戻り型? { 本体ブロック }。
            Some(Token::Fn) => {
                self.expect(Token::LParen)?;
                let mut params = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    loop {
                        let name = match self.next() {
                            Some(Token::Ident(name)) => name,
                            Some(other) => {
                                return Err(AwaError::UnexpectedToken(format!("{:?}", other)));
                            }
                            None => return Err(AwaError::UnexpectedEof),
                        };
                        // 任意の引数型注釈 n: Num
                        let annotation = if self.peek() == Some(&Token::Colon) {
                            self.next(); // ':'
                            Some(self.parse_type_expr()?)
                        } else {
                            None
                        };
                        params.push((name, annotation));
                        match self.peek() {
                            Some(Token::Comma) => {
                                self.next();
                            }
                            _ => break,
                        }
                    }
                }
                self.expect(Token::RParen)?;
                // 任意の戻り型注釈 -> T
                let ret_annotation = if self.peek() == Some(&Token::Arrow) {
                    self.next(); // '->'
                    Some(self.parse_type_expr()?)
                } else {
                    None
                };
                self.expect(Token::LBrace)?;
                let body = self.parse_block()?;
                Ok(Expr::Lambda(params, ret_annotation, Box::new(body)))
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
            // パターンマッチ case 主語 { パターン -> 本体; ... }。
            Some(Token::Case) => {
                let subject = self.parse_expr()?;
                self.expect(Token::LBrace)?;
                let mut arms = Vec::new();
                self.skip_terminators();
                while self.peek() != Some(&Token::RBrace) {
                    if self.peek().is_none() {
                        return Err(AwaError::UnexpectedEof);
                    }
                    let pat = self.parse_pattern()?;
                    self.expect(Token::Arrow)?;
                    let body = self.parse_expr()?;
                    arms.push((pat, body));
                    match self.peek() {
                        Some(Token::Terminator) => self.skip_terminators(),
                        Some(Token::RBrace) => break,
                        None => return Err(AwaError::UnexpectedEof),
                        Some(other) => {
                            return Err(AwaError::UnexpectedToken(format!("{:?}", other)));
                        }
                    }
                }
                self.expect(Token::RBrace)?;
                Ok(Expr::Case(Box::new(subject), arms))
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
