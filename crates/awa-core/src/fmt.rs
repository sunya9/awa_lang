// ===== フォーマッタ（opinionated・幅 100・インデント 2）=====
// ソース → AST（parse_module）→ Doc（Wadler 風の幅優先プリンタ）→ 文字列。
// コメントは lexer が別に収集したものを、文/行の位置（span）を頼りに
// 「先頭コメント（その行の上）」「行末コメント（同じ行）」として織り込む。
// 既知の割り切り: 型本体の中のコメントは織り込まない（型宣言の前後へ寄る）。
// 三連引用符は通常の文字列（\n 等のエスケープ）として再出力される。
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::{Expr, ExprKind, ImplBlock, Import, Method, ParsedModule, Pattern, Stmt, TypeDef, TypeExpr, TypeExprKind};
use crate::error::LocatedError;
use crate::lexer::{Comment, tokenize_with_comments};
use crate::parser::Parser;
use crate::token::Token;

const WIDTH: usize = 100;
const INDENT: usize = 2;

// ---- Doc 代数 ----
enum Doc {
    Nil,
    Text(String),
    Line, // flat: " " / break: 改行+インデント
    Soft, // flat: ""  / break: 改行+インデント
    Hard, // 常に改行+インデント
    Cat(D, D),
    Nest(usize, D),
    Group(D),
}
type D = Rc<Doc>;

fn nil() -> D { Rc::new(Doc::Nil) }
fn text<S: Into<String>>(s: S) -> D { Rc::new(Doc::Text(s.into())) }
fn line() -> D { Rc::new(Doc::Line) }
fn soft() -> D { Rc::new(Doc::Soft) }
fn hard() -> D { Rc::new(Doc::Hard) }
fn cat(a: D, b: D) -> D { Rc::new(Doc::Cat(a, b)) }
fn nest(n: usize, d: D) -> D { Rc::new(Doc::Nest(n, d)) }
fn group(d: D) -> D { Rc::new(Doc::Group(d)) }
fn concat(items: Vec<D>) -> D { items.into_iter().fold(nil(), cat) }

fn join(items: Vec<D>, sep: D) -> D {
    let mut out = nil();
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            out = cat(out, sep.clone());
        }
        out = cat(out, it);
    }
    out
}

// flat にしたときの幅。Hard を含むと flat 不可（None）。
fn flat_len(d: &Doc) -> Option<usize> {
    match d {
        Doc::Nil | Doc::Soft => Some(0),
        Doc::Text(s) => Some(s.chars().count()),
        Doc::Line => Some(1),
        Doc::Hard => None,
        Doc::Cat(a, b) => Some(flat_len(a)? + flat_len(b)?),
        Doc::Nest(_, x) | Doc::Group(x) => flat_len(x),
    }
}

fn render(doc: &D, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut stack: Vec<(usize, bool, D)> = vec![(0, false, doc.clone())];
    while let Some((ind, flat, d)) = stack.pop() {
        match &*d {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(s);
                col += s.chars().count();
            }
            Doc::Line => {
                if flat {
                    out.push(' ');
                    col += 1;
                } else {
                    out.push('\n');
                    out.push_str(&" ".repeat(ind));
                    col = ind;
                }
            }
            Doc::Soft => {
                if !flat {
                    out.push('\n');
                    out.push_str(&" ".repeat(ind));
                    col = ind;
                }
            }
            Doc::Hard => {
                out.push('\n');
                out.push_str(&" ".repeat(ind));
                col = ind;
            }
            Doc::Cat(a, b) => {
                stack.push((ind, flat, b.clone()));
                stack.push((ind, flat, a.clone()));
            }
            Doc::Nest(j, x) => stack.push((ind + j, flat, x.clone())),
            Doc::Group(x) => {
                let fits = flat || matches!(flat_len(x), Some(w) if col + w <= width);
                stack.push((ind, fits, x.clone()));
            }
        }
    }
    out
}

// ---- 文字列/数値リテラルの再出力 ----
// 文字列の中身をエスケープ（クォートは付けない）。`${` に化ける `$`（直後が `{`）だけ
// `\$` にして、再 lex で意図せず補間にならないようにする。
fn escape_inner(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            _ => out.push(c),
        }
    }
    out
}

fn quote_str(s: &str) -> String {
    format!("\"{}\"", escape_inner(s))
}

fn fmt_float(f: f64) -> String {
    let s = format!("{}", f);
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{}.0", s) // Float が Int に化けないよう小数点を補う
    }
}

fn binop_prec(t: &Token) -> u8 {
    match t {
        Token::OrOr => 1,
        Token::AndAnd => 2,
        Token::EqEq | Token::NotEq => 3,
        Token::Lt | Token::Gt | Token::Le | Token::Ge | Token::Concat => 4,
        Token::Plus | Token::Minus => 5,
        Token::Star | Token::Slash => 6,
        _ => 0,
    }
}

fn binop_str(t: &Token) -> &'static str {
    match t {
        Token::Plus => "+",
        Token::Minus => "-",
        Token::Star => "*",
        Token::Slash => "/",
        Token::EqEq => "==",
        Token::NotEq => "!=",
        Token::Lt => "<",
        Token::Gt => ">",
        Token::Le => "<=",
        Token::Ge => ">=",
        Token::Concat => "<>",
        Token::AndAnd => "&&",
        Token::OrOr => "||",
        _ => "?",
    }
}

// 文の [start, end) 範囲（コメントの帰属判定用）。
fn stmt_bounds(s: &Stmt) -> (usize, usize) {
    match s {
        Stmt::Let(_, ann, e) => (
            ann.as_ref().map(|a| a.span.start).unwrap_or(e.span.start),
            e.span.end,
        ),
        Stmt::Expr(e) => (e.span.start, e.span.end),
        Stmt::Type(d) => (d.span.start, d.span.end),
        Stmt::Impl(b) => (b.span.start, b.span.end),
    }
}

fn comment_doc(c: &Comment) -> D {
    if c.block {
        text(c.text.clone())
    } else {
        let body = c.text.trim_start_matches('/').trim_start();
        if body.is_empty() {
            text("//")
        } else {
            text(format!("// {}", body))
        }
    }
}

struct Fmt {
    comments: Vec<Comment>,
    ci: usize,
    line_starts: Vec<usize>,
    exports: HashSet<String>,
    last_line: usize, // 直近に出力した構文要素の終端行（空行保持の判定用）
}

impl Fmt {
    fn line_of(&self, off: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= off)
    }

    // 連続する項目の区切り。基本は改行 1 つだが、ソースで項目（または直前コメント）との間に
    // 空行があれば改行 2 つにして 1 行ぶんの空行を残す（ユーザーの段落分けを保持）。
    fn sep_before(&mut self, item_start: usize) -> D {
        let next_line = match self.comments.get(self.ci) {
            Some(c) if c.span.start < item_start => self.line_of(c.span.start),
            _ => self.line_of(item_start),
        };
        if self.last_line > 0 && next_line > self.last_line + 1 {
            cat(hard(), hard())
        } else {
            hard()
        }
    }

    // off より前（前の行）にあるコメントを、それぞれ独立行として吐く。
    fn leading(&mut self, off: usize) -> D {
        let mut docs = vec![];
        while self.ci < self.comments.len() && self.comments[self.ci].span.start < off {
            docs.push(comment_doc(&self.comments[self.ci]));
            docs.push(hard());
            self.ci += 1;
        }
        concat(docs)
    }

    // end_off と同じ行にあるコメントを行末コメントとして吐く（2 スペース空け）。
    fn trailing(&mut self, end_off: usize) -> D {
        if self.ci < self.comments.len() {
            let c = &self.comments[self.ci];
            if self.line_of(c.span.start) == self.line_of(end_off.saturating_sub(1)) {
                let d = cat(text("  "), comment_doc(c));
                self.ci += 1;
                return d;
            }
        }
        nil()
    }

    // 残りのコメント（ファイル末尾など）。
    fn rest(&mut self) -> D {
        let mut docs = vec![];
        while self.ci < self.comments.len() {
            docs.push(hard());
            docs.push(comment_doc(&self.comments[self.ci]));
            self.ci += 1;
        }
        concat(docs)
    }

    fn is_pub(&self, s: &Stmt) -> bool {
        match s {
            Stmt::Let(name, _, _) => self.exports.contains(name),
            Stmt::Type(d) => self.exports.contains(&d.name),
            Stmt::Impl(b) => b.is_pub,
            Stmt::Expr(_) => false,
        }
    }

    fn module(&mut self, m: &ParsedModule) -> D {
        let mut parts = vec![];
        let mut first = true;
        for imp in &m.imports {
            if first {
                first = false;
            } else {
                parts.push(self.sep_before(imp.span.start));
            }
            parts.push(self.leading(imp.span.start));
            parts.push(import_doc(imp));
            parts.push(self.trailing(imp.span.end));
            self.last_line = self.line_of(imp.span.end);
        }
        for s in &m.stmts {
            let (start, end) = stmt_bounds(s);
            if first {
                first = false;
            } else {
                parts.push(self.sep_before(start));
            }
            parts.push(self.leading(start));
            let p = self.is_pub(s);
            parts.push(self.stmt(s, p));
            parts.push(self.trailing(end));
            self.last_line = self.line_of(end);
        }
        parts.push(self.rest());
        concat(parts)
    }

    fn stmt(&mut self, s: &Stmt, is_pub: bool) -> D {
        let pre = if is_pub { text("pub ") } else { nil() };
        match s {
            Stmt::Let(name, ann, e) => {
                let ann_doc = match ann {
                    Some(t) => cat(text(": "), type_doc(t)),
                    None => nil(),
                };
                concat(vec![
                    pre,
                    text("let "),
                    text(name.clone()),
                    ann_doc,
                    text(" = "),
                    self.expr(e),
                ])
            }
            Stmt::Expr(e) => self.expr(e),
            Stmt::Type(d) => cat(pre, self.type_def(d)),
            Stmt::Impl(b) => cat(pre, self.impl_block(b)),
        }
    }

    fn type_def(&mut self, d: &TypeDef) -> D {
        let params = if d.params.is_empty() {
            nil()
        } else {
            concat(vec![text("("), text(d.params.join(", ")), text(")")])
        };
        let variants: Vec<D> = d
            .variants
            .iter()
            .map(|v| {
                if v.fields.is_empty() {
                    text(v.name.clone())
                } else {
                    let fs: Vec<D> = v.fields.iter().map(type_doc).collect();
                    concat(vec![text(v.name.clone()), text("("), join(fs, text(", ")), text(")")])
                }
            })
            .collect();
        let header = concat(vec![text("type "), text(d.name.clone()), params]);
        braced(header, concat(vec![join(variants, hard())]))
    }

    fn impl_block(&mut self, b: &ImplBlock) -> D {
        let params = if b.params.is_empty() {
            nil()
        } else {
            concat(vec![text("("), text(b.params.join(", ")), text(")")])
        };
        let header = concat(vec![text("impl "), text(b.type_name.clone()), params]);
        let mut parts = vec![];
        for (i, mth) in b.methods.iter().enumerate() {
            if i > 0 {
                parts.push(self.sep_before(mth.body.span.start));
            }
            parts.push(self.leading(mth.body.span.start));
            // pub impl は impl 側で公開済み。メソッド個別の pub fn だけを再出力する。
            parts.push(self.method(mth, mth.is_pub));
            parts.push(self.trailing(mth.body.span.end));
            self.last_line = self.line_of(mth.body.span.end);
        }
        braced(header, concat(parts))
    }

    fn method(&mut self, m: &Method, is_pub: bool) -> D {
        let pre = if is_pub { text("pub ") } else { nil() };
        let params: Vec<D> = m
            .params
            .iter()
            .map(|(n, ann)| match ann {
                Some(t) => concat(vec![text(n.clone()), text(": "), type_doc(t)]),
                None => text(n.clone()),
            })
            .collect();
        let ret = match &m.ret {
            Some(t) => concat(vec![text(" -> "), type_doc(t)]),
            None => nil(),
        };
        concat(vec![
            pre,
            text("fn "),
            text(m.name.clone()),
            text("("),
            join(params, text(", ")),
            text(")"),
            ret,
            text(" "),
            self.expr(&m.body),
        ])
    }

    fn expr(&mut self, e: &Expr) -> D {
        match &e.kind {
            ExprKind::Int(n) => text(n.to_string()),
            ExprKind::Long(n) => text(format!("{}L", n)),
            ExprKind::Float(f) => text(fmt_float(*f)),
            ExprKind::Str(s) => text(quote_str(s)),
            ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
            ExprKind::Var(n) => text(n.clone()),
            ExprKind::Neg(x) => cat(text("-"), self.atom(x)),
            ExprKind::BinOp(l, op, r) => {
                // `${}` 補間（lexer が `( "a" <> __interp(e) <> "b" )` へ脱糖）を文字列へ復元する。
                if matches!(op, Token::Concat) {
                    if let Some(doc) = self.try_interp(e) {
                        return doc;
                    }
                }
                let p = binop_prec(op);
                concat(vec![
                    self.binop_operand(l, p, false),
                    text(format!(" {} ", binop_str(op))),
                    self.binop_operand(r, p, true),
                ])
            }
            ExprKind::Lambda(params, ret, body) => {
                let ps: Vec<D> = params
                    .iter()
                    .map(|(n, ann)| match ann {
                        Some(t) => concat(vec![text(n.clone()), text(": "), type_doc(t)]),
                        None => text(n.clone()),
                    })
                    .collect();
                let ret_doc = match ret {
                    Some(t) => concat(vec![text(" -> "), type_doc(t)]),
                    None => nil(),
                };
                concat(vec![
                    text("fn("),
                    join(ps, text(", ")),
                    text(")"),
                    ret_doc,
                    text(" "),
                    self.lambda_body(body),
                ])
            }
            ExprKind::Call(callee, args) => {
                let cd = self.callee(callee);
                self.call_like(cd, args)
            }
            ExprKind::MethodCall(recv, name, args) => {
                // do は parser で `recv.flatMap(fn(v){...})` に脱糖されるので、
                // その形を見つけたら do 記法へ復元する（整形で do が消えないように）。
                if name == "flatMap" {
                    let mut items = vec![];
                    if self.collect_do(e, &mut items) {
                        return braced(text("do"), join(items, hard()));
                    }
                }
                let cd = concat(vec![self.callee(recv), text("."), text(name.clone())]);
                self.call_like(cd, args)
            }
            ExprKind::Field(recv, name) => {
                concat(vec![self.callee(recv), text("."), text(name.clone())])
            }
            ExprKind::ArrayLit(elems) => {
                let docs: Vec<D> = elems.iter().map(|x| self.expr(x)).collect();
                concat(vec![text("["), join(docs, text(", ")), text("]")])
            }
            ExprKind::TupleLit(elems) => {
                let docs: Vec<D> = elems.iter().map(|x| self.expr(x)).collect();
                concat(vec![text("("), join(docs, text(", ")), text(")")])
            }
            ExprKind::Block(stmts) => self.block(stmts, e.span.end),
            ExprKind::Case(subj, arms) => {
                let header = concat(vec![text("case "), self.expr(subj)]);
                let mut parts = vec![];
                for (i, (pat, body)) in arms.iter().enumerate() {
                    if i > 0 {
                        parts.push(self.sep_before(body.span.start));
                    }
                    parts.push(self.leading(body.span.start));
                    parts.push(concat(vec![pattern_doc(pat), text(" -> "), self.expr(body)]));
                    parts.push(self.trailing(body.span.end));
                    self.last_line = self.line_of(body.span.end);
                }
                braced(header, concat(parts))
            }
        }
    }

    // `<>` 連結が `${}` 補間の脱糖（文字列リテラルと __interp(e) の連結）なら、補間文字列へ復元。
    // 純粋な補間（リテラルと __interp のみ・__interp が 1 つ以上）でなければ None。
    fn try_interp(&mut self, e: &Expr) -> Option<D> {
        let mut parts: Vec<InterpPart> = Vec::new();
        if !flatten_concat(e, &mut parts) {
            return None;
        }
        if !parts.iter().any(|p| matches!(p, InterpPart::Interp(_))) {
            return None;
        }
        let mut s = String::from("\"");
        for p in parts {
            match p {
                InterpPart::Lit(lit) => s.push_str(&escape_inner(lit)),
                InterpPart::Interp(inner) => {
                    let flat = render(&self.expr(inner), usize::MAX);
                    s.push_str("${");
                    s.push_str(flat.trim_end_matches('\n'));
                    s.push('}');
                }
            }
        }
        s.push('"');
        Some(text(s))
    }

    // ラムダ本体。Block ならそのまま、裸の式なら波括弧で包む（awa の fn は本体に { } 必須）。
    fn lambda_body(&mut self, body: &Expr) -> D {
        if matches!(body.kind, ExprKind::Block(_)) {
            self.expr(body)
        } else {
            group(concat(vec![
                text("{"),
                nest(INDENT, cat(line(), self.expr(body))),
                line(),
                text("}"),
            ]))
        }
    }

    // flatMap チェーンを do 項目へ畳む。do でなければ false（items は使わない）。
    fn collect_do(&mut self, e: &Expr, items: &mut Vec<D>) -> bool {
        if let ExprKind::MethodCall(recv, name, args) = &e.kind {
            if name == "flatMap" && args.len() == 1 {
                if let ExprKind::Lambda(ps, None, body) = &args[0].kind {
                    if ps.len() == 1 && ps[0].1.is_none() {
                        let v = &ps[0].0;
                        if v == "_" {
                            items.push(self.expr(recv)); // 途中の式（結果を捨てる）
                        } else {
                            items.push(concat(vec![text(v.clone()), text(" <- "), self.expr(recv)]));
                        }
                        self.collect_do_cont(body, items);
                        return true;
                    }
                }
            }
        }
        false
    }

    // do の継続部: さらなる bind / `let x = e`（2 文ブロックの脱糖）/ 末尾式。
    fn collect_do_cont(&mut self, e: &Expr, items: &mut Vec<D>) {
        if self.collect_do(e, items) {
            return;
        }
        if let ExprKind::Block(stmts) = &e.kind {
            if stmts.len() == 2 {
                if let (Stmt::Let(x, None, v), Stmt::Expr(rest)) = (&stmts[0], &stmts[1]) {
                    items.push(concat(vec![text("let "), text(x.clone()), text(" = "), self.expr(v)]));
                    self.collect_do_cont(rest, items);
                    return;
                }
            }
        }
        items.push(self.expr(e)); // 末尾のモナド式
    }

    // callee/receiver は二項演算・ラムダ・if・case を括弧で包む（それ以外は素）。
    fn callee(&mut self, e: &Expr) -> D {
        if matches!(
            e.kind,
            ExprKind::BinOp(..)
                | ExprKind::Neg(..)
                | ExprKind::Lambda(..)
                | ExprKind::Case(..)
        ) {
            concat(vec![text("("), self.expr(e), text(")")])
        } else {
            self.expr(e)
        }
    }

    // 単項マイナスの被演算子（二項演算なら括弧）。
    fn atom(&mut self, e: &Expr) -> D {
        if matches!(e.kind, ExprKind::BinOp(..)) {
            concat(vec![text("("), self.expr(e), text(")")])
        } else {
            self.expr(e)
        }
    }

    fn binop_operand(&mut self, e: &Expr, parent: u8, is_right: bool) -> D {
        if let ExprKind::BinOp(_, op, _) = &e.kind {
            let q = binop_prec(op);
            if q < parent || (q == parent && is_right) {
                return concat(vec![text("("), self.expr(e), text(")")]);
            }
        }
        self.expr(e)
    }

    // f(args) / recv.m(args)。幅を超えたら 1 引数 1 行へ折る。
    fn call_like(&mut self, head: D, args: &[Expr]) -> D {
        if args.is_empty() {
            return concat(vec![head, text("()")]);
        }
        let arg_docs: Vec<D> = args.iter().map(|a| self.expr(a)).collect();
        group(concat(vec![
            head,
            text("("),
            nest(INDENT, concat(vec![soft(), join(arg_docs, concat(vec![text(","), line()]))])),
            soft(),
            text(")"),
        ]))
    }

    // ブロック { ... }。単文かつ収まれば 1 行、そうでなければ複数行。close は '}' の位置。
    fn block(&mut self, stmts: &[Stmt], close: usize) -> D {
        if stmts.is_empty() {
            // 末尾までのコメントだけ拾って空ブロックに。
            let _ = self.leading(close);
            return text("{}");
        }
        let mut parts = vec![];
        for (i, s) in stmts.iter().enumerate() {
            let (start, end) = stmt_bounds(s);
            if i > 0 {
                parts.push(self.sep_before(start));
            }
            parts.push(self.leading(start));
            parts.push(self.stmt(s, false));
            parts.push(self.trailing(end));
            self.last_line = self.line_of(end);
        }
        // 最後の文と '}' の間のコメント。
        parts.push(self.leading(close));
        let inner = concat(parts);
        group(concat(vec![
            text("{"),
            nest(INDENT, concat(vec![line(), inner])),
            line(),
            text("}"),
        ]))
    }
}

// 常に複数行の波括弧ブロック（type/impl/case の本体）。
fn braced(header: D, inner: D) -> D {
    concat(vec![
        header,
        text(" {"),
        nest(INDENT, cat(hard(), inner)),
        hard(),
        text("}"),
    ])
}

fn import_doc(imp: &Import) -> D {
    let path = imp.path.join("/");
    match &imp.alias {
        Some(a) => text(format!("import {} as {}", path, a)),
        None => text(format!("import {}", path)),
    }
}

fn type_doc(t: &TypeExpr) -> D {
    match &t.kind {
        TypeExprKind::Var(n) => text(n.clone()),
        // タプル型は (A, B) と表示する（Tuple(A, B) ではなく）。
        TypeExprKind::Con(name, args) if name == "Tuple" => {
            let a: Vec<D> = args.iter().map(type_doc).collect();
            concat(vec![text("("), join(a, text(", ")), text(")")])
        }
        TypeExprKind::Con(name, args) => {
            if args.is_empty() {
                text(name.clone())
            } else {
                let a: Vec<D> = args.iter().map(type_doc).collect();
                concat(vec![text(name.clone()), text("("), join(a, text(", ")), text(")")])
            }
        }
        TypeExprKind::Fun(params, ret) => {
            let ps: Vec<D> = params.iter().map(type_doc).collect();
            concat(vec![
                text("fn("),
                join(ps, text(", ")),
                text(") -> "),
                type_doc(ret),
            ])
        }
    }
}

// `${}` 補間復元のための連結要素。
enum InterpPart<'a> {
    Lit(&'a str),
    Interp(&'a Expr),
}

// `<>` 連結木を左から右へ平坦化する。葉が文字列リテラルか __interp(e) 呼び出しなら収集して
// true、それ以外（ふつうの `<>` 連結のオペランド）が混ざっていれば false。
fn flatten_concat<'a>(e: &'a Expr, out: &mut Vec<InterpPart<'a>>) -> bool {
    match &e.kind {
        ExprKind::BinOp(l, Token::Concat, r) => {
            flatten_concat(l, out) && flatten_concat(r, out)
        }
        ExprKind::Str(s) => {
            out.push(InterpPart::Lit(s));
            true
        }
        ExprKind::Call(callee, args)
            if args.len() == 1 && matches!(&callee.kind, ExprKind::Var(n) if n == "__interp") =>
        {
            out.push(InterpPart::Interp(&args[0]));
            true
        }
        _ => false,
    }
}

fn pattern_doc(p: &Pattern) -> D {
    match p {
        Pattern::Wildcard => text("_"),
        Pattern::Var(n) => text(n.clone()),
        Pattern::Bool(b) => text(if *b { "true" } else { "false" }),
        Pattern::Int(n) => text(n.to_string()),
        Pattern::Str(s) => text(quote_str(s)),
        Pattern::Array(subs) => {
            let s: Vec<D> = subs.iter().map(pattern_doc).collect();
            concat(vec![text("["), join(s, text(", ")), text("]")])
        }
        Pattern::Tuple(subs) => {
            let s: Vec<D> = subs.iter().map(pattern_doc).collect();
            concat(vec![text("("), join(s, text(", ")), text(")")])
        }
        Pattern::Ctor(name, subs) => {
            if subs.is_empty() {
                text(name.clone())
            } else {
                let s: Vec<D> = subs.iter().map(pattern_doc).collect();
                concat(vec![text(name.clone()), text("("), join(s, text(", ")), text(")")])
            }
        }
        Pattern::Or(alts) => {
            let a: Vec<D> = alts.iter().map(pattern_doc).collect();
            join(a, text(" | "))
        }
    }
}

fn compute_line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, c) in src.chars().enumerate() {
        if c == '\n' {
            v.push(i + 1);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::format_source;

    fn fmt(src: &str) -> String {
        format_source(src).expect("整形成功")
    }

    #[test]
    fn 空白と演算子が正規化される() {
        assert_eq!(fmt("let  x=1+2*3"), "let x = 1 + 2 * 3\n");
    }

    #[test]
    fn 優先順位に応じて括弧を保つ() {
        // (1 + 2) * 3 は掛け算より弱い加算を括弧で保つ。
        assert_eq!(fmt("(1 + 2) * 3"), "(1 + 2) * 3\n");
        // 1 + 2 * 3 は括弧不要。
        assert_eq!(fmt("1 + 2 * 3"), "1 + 2 * 3\n");
    }

    #[test]
    fn 論理演算子の優先順位で括弧を保つ() {
        // || は && より緩い → (a && b) || c は括弧不要。
        assert_eq!(fmt("a && b || c"), "a && b || c\n");
        // a && (b || c) は括弧が必要。
        assert_eq!(fmt("a && (b || c)"), "a && (b || c)\n");
    }

    #[test]
    fn 行末コメントが保たれる() {
        assert_eq!(fmt("let x = 1 // hi"), "let x = 1  // hi\n");
    }

    #[test]
    fn 先頭コメントが保たれる() {
        assert_eq!(fmt("// doc\nlet x = 1"), "// doc\nlet x = 1\n");
    }

    #[test]
    fn type_は複数行に展開される() {
        assert_eq!(
            fmt("type Option(a) { Some(a); None }"),
            "type Option(a) {\n  Some(a)\n  None\n}\n"
        );
    }

    #[test]
    fn case_は複数行に展開される() {
        let out = fmt("case x { Some(n) -> n; None -> 0 }");
        assert_eq!(out, "case x {\n  Some(n) -> n\n  None -> 0\n}\n");
    }

    #[test]
    fn do記法が復元される() {
        // parser は do を flatMap へ脱糖するが、フォーマッタは do へ戻す。
        let out = fmt("do { x <- foo(1); bar(x) }");
        assert_eq!(out, "do {\n  x <- foo(1)\n  bar(x)\n}\n");
    }

    #[test]
    fn 文字列エスケープが再生成される() {
        assert_eq!(fmt("\"a\\nb\""), "\"a\\nb\"\n");
    }

    #[test]
    fn 文字列補間が復元される() {
        // ${} は lexer で __interp へ脱糖されるが、fmt は補間文字列へ戻す。
        assert_eq!(fmt("let x = 1\n\"n=${x + 2}\""), "let x = 1\n\"n=${x + 2}\"\n");
        // リテラルの $ は \$ に保つ（補間に化けない）。
        assert_eq!(fmt("\"price \\${5}\""), "\"price \\${5}\"\n");
        // 補間でない素の連結はそのまま。
        assert_eq!(fmt("\"a\" <> \"b\""), "\"a\" <> \"b\"\n");
    }

    #[test]
    fn ソースの空行を1行だけ保持する() {
        assert_eq!(fmt("let a = 1\nlet b = 2"), "let a = 1\nlet b = 2\n"); // 隣接は詰める
        assert_eq!(fmt("let a = 1\n\nlet b = 2"), "let a = 1\n\nlet b = 2\n"); // 空行は残す
        assert_eq!(fmt("let a = 1\n\n\n\nlet b = 2"), "let a = 1\n\nlet b = 2\n"); // 連続は 1 つに
    }

    #[test]
    fn 整形は冪等() {
        let src = "pub let f = fn(x: Int) -> Int {\n  case x {\n    0 -> 1\n    n -> n * f(n - 1)\n  }\n}\n";
        let once = fmt(src);
        assert_eq!(once, fmt(&once));
    }
}

// ソースを整形して返す。構文エラーなら位置つきで返す（整形できない）。
pub fn format_source(src: &str) -> Result<String, LocatedError> {
    let (tokens, mut comments) = tokenize_with_comments(src)?;
    let module = Parser::new(tokens).parse_module()?;
    comments.sort_by_key(|c| c.span.start);
    let mut f = Fmt {
        comments,
        ci: 0,
        line_starts: compute_line_starts(src),
        exports: module.exports.clone(),
        last_line: 0,
    };
    let doc = f.module(&module);
    let rendered = render(&doc, WIDTH);
    // 各行の末尾空白を除去（空行に残るインデント等を消す）。
    let mut out: String = rendered
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    Ok(out)
}
