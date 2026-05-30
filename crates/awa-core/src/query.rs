// ===== 位置クエリ（LSP の hover / 定義ジャンプ / 補完を支える）=====
// ソースとカーソル位置（char オフセット）から「その式の型」「定義位置」「補完候補」を返す。
// 型検査（check_program）の型付き AST と、字句のトークン列を使う。
use crate::ast::{Stmt, TExpr, TExprKind, TStmt};
use crate::infer::Checker;
use crate::lexer::{tokenize, tokenize_with_comments};
use crate::parser::Parser;
use crate::span::Span;
use crate::token::Token;
use crate::types::{Type, render_type};

// モジュールの公開メンバー（module.X でアクセスできるもの）。
#[derive(Debug, Clone, PartialEq)]
pub enum MemberKind {
    Value,
    Type,
    Constructor,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub label: String,
    pub kind: MemberKind,
    pub detail: Option<String>, // シグネチャ（型注釈や構築子フィールド）
    pub doc: Option<String>,    // 直前の // コメント（ドキュメント）
}

// 文書内の import から、修飾子（別名 or 末尾セグメント）に対応するモジュールパスを引く。
// 例: `import std/option` があるとき "option" -> ["std","option"]。
// 編集中の文書はカーソル位置で構文的に不完全なことが多いので、全体をパースせず
// トークン列を走査して import 文だけを拾う（lex は通る前提）。
pub fn module_qualifier_path(src: &str, qualifier: &str) -> Option<Vec<String>> {
    let tokens = tokenize(src).ok()?;
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].node != Token::Import {
            i += 1;
            continue;
        }
        i += 1;
        // seg (/ seg)*
        let mut path = vec![];
        while let Some(Token::Ident(s)) = tokens.get(i).map(|t| &t.node) {
            path.push(s.clone());
            i += 1;
            if tokens.get(i).map(|t| &t.node) == Some(&Token::Slash) {
                i += 1;
            } else {
                break;
            }
        }
        // 任意の `as alias`
        let mut alias = None;
        if tokens.get(i).map(|t| &t.node) == Some(&Token::As) {
            i += 1;
            if let Some(Token::Ident(a)) = tokens.get(i).map(|t| &t.node) {
                alias = Some(a.clone());
                i += 1;
            }
        }
        let qual = alias.as_deref().or_else(|| path.last().map(|s| s.as_str()));
        if qual == Some(qualifier) {
            return Some(path);
        }
    }
    None
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, c) in src.chars().enumerate() {
        if c == '\n' {
            v.push(i + 1);
        }
    }
    v
}

// モジュールソースから、module.X でアクセスできる公開メンバーを集める。
// pub let（値）、pub type（型名）、pub 型の構築子。各メンバーに直前の行コメントを添える。
// メソッド（impl）は receiver.method で呼ぶもので module.method ではないため含めない。
pub fn module_members(src: &str) -> Vec<Member> {
    let Ok((tokens, mut comments)) = tokenize_with_comments(src) else {
        return vec![];
    };
    let Ok(module) = Parser::new(tokens).parse_module() else {
        return vec![];
    };
    comments.sort_by_key(|c| c.span.start);
    let chars: Vec<char> = src.chars().collect();
    let starts = line_starts(src);
    let line_of = |off: usize| starts.partition_point(|&s| s <= off);
    let slice = |sp: Span| {
        chars[sp.start.min(chars.len())..sp.end.min(chars.len())]
            .iter()
            .collect::<String>()
    };
    // 定義の開始行のすぐ上から連続する行コメントをドキュメントとして集める。
    let doc_above = |start: usize| -> Option<String> {
        let target = line_of(start) as i64;
        let mut lines: Vec<String> = vec![];
        let mut ln = target - 1;
        while ln >= 1 {
            match comments
                .iter()
                .find(|c| !c.block && line_of(c.span.start) as i64 == ln)
            {
                Some(c) => {
                    lines.push(c.text.trim_start_matches('/').trim().to_string());
                    ln -= 1;
                }
                None => break,
            }
        }
        if lines.is_empty() {
            None
        } else {
            lines.reverse();
            Some(lines.join("\n"))
        }
    };

    let mut out = vec![];
    for s in &module.stmts {
        match s {
            Stmt::Let(name, ann, e) if module.exports.contains(name) => {
                out.push(Member {
                    label: name.clone(),
                    kind: MemberKind::Value,
                    detail: ann.as_ref().map(|a| slice(a.span)),
                    doc: doc_above(e.span.start),
                });
            }
            Stmt::Type(def) if module.exports.contains(&def.name) => {
                let params = if def.params.is_empty() {
                    String::new()
                } else {
                    format!("({})", def.params.join(", "))
                };
                out.push(Member {
                    label: def.name.clone(),
                    kind: MemberKind::Type,
                    detail: Some(format!("type {}{}", def.name, params)),
                    doc: doc_above(def.span.start),
                });
                // pub 型はその構築子も公開される。
                for v in &def.variants {
                    let sig = if v.fields.is_empty() {
                        v.name.clone()
                    } else {
                        let fs: Vec<String> = v.fields.iter().map(|f| slice(f.span)).collect();
                        format!("{}({})", v.name, fs.join(", "))
                    };
                    out.push(Member {
                        label: v.name.clone(),
                        kind: MemberKind::Constructor,
                        detail: Some(sig),
                        doc: None,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

// オフセット位置にある識別子（名前とその span）。トークン列から探す。
pub fn ident_at(source: &str, offset: usize) -> Option<(String, Span)> {
    let tokens = tokenize(source).ok()?;
    for t in &tokens {
        if let Token::Ident(name) = &t.node {
            if offset >= t.span.start && offset < t.span.end {
                return Some((name.clone(), t.span));
            }
        }
    }
    None
}

// hover: オフセットを含む最小の式ノードの型を返す（型検査が通る時のみ）。
pub fn type_at(source: &str, offset: usize) -> Option<String> {
    let tokens = tokenize(source).ok()?;
    let program = Parser::new(tokens).parse_program().ok()?;
    let mut checker = Checker::new();
    let typed = checker.check_program(&program).ok()?;
    let mut best: Option<(Span, Type)> = None;
    for ts in &typed {
        smallest_in_stmt(ts, offset, &mut best);
    }
    best.map(|(_, ty)| render_type(&ty))
}

fn smallest_in_stmt(ts: &TStmt, offset: usize, best: &mut Option<(Span, Type)>) {
    match ts {
        TStmt::Let(_, te) | TStmt::Expr(te) => smallest_in_expr(te, offset, best),
        TStmt::Impl(methods) => {
            for m in methods {
                smallest_in_expr(&m.body, offset, best);
            }
        }
        TStmt::Type(_) => {}
    }
}

// offset を含むノードのうち span が最小のものの (span, type) を best に集める。
fn smallest_in_expr(te: &TExpr, offset: usize, best: &mut Option<(Span, Type)>) {
    let sp = te.span;
    if offset < sp.start || offset >= sp.end {
        return; // この部分木は範囲外
    }
    let len = sp.end - sp.start;
    let better = match best {
        Some((bsp, _)) => len < (bsp.end - bsp.start),
        None => true,
    };
    if better {
        *best = Some((sp, te.ty.clone()));
    }
    // 子ノードへ（より小さい span で上書きされうる）。
    match &te.kind {
        TExprKind::Neg(x) | TExprKind::BitNot(x) => smallest_in_expr(x, offset, best),
        TExprKind::BinOp(l, _, r) => {
            smallest_in_expr(l, offset, best);
            smallest_in_expr(r, offset, best);
        }
        TExprKind::Lambda(_, body) => smallest_in_expr(body, offset, best),
        TExprKind::Call(callee, args) => {
            smallest_in_expr(callee, offset, best);
            for a in args {
                smallest_in_expr(a, offset, best);
            }
        }
        TExprKind::Block(stmts) => {
            for s in stmts {
                smallest_in_stmt(s, offset, best);
            }
        }
        TExprKind::Case(subj, arms) => {
            smallest_in_expr(subj, offset, best);
            for (_, body) in arms {
                smallest_in_expr(body, offset, best);
            }
        }
        TExprKind::ArrayLit(elems) | TExprKind::TupleLit(elems) => {
            for e in elems {
                smallest_in_expr(e, offset, best);
            }
        }
        TExprKind::Int(_)
        | TExprKind::Long(_)
        | TExprKind::Float(_)
        | TExprKind::Str(_)
        | TExprKind::Bool(_)
        | TExprKind::Var(_) => {}
    }
}

// 定義ジャンプ: カーソル位置の名前のトップレベル定義（let/type/メソッド fn）の span を返す。
// トークン列で `Let Ident(name)` / `Type Ident(name)` / `Fn Ident(name)` を探す（v1・トップレベル）。
pub fn definition_at(source: &str, offset: usize) -> Option<Span> {
    let (name, _) = ident_at(source, offset)?;
    let tokens = tokenize(source).ok()?;
    for w in tokens.windows(2) {
        if matches!(w[0].node, Token::Let | Token::Type | Token::Fn) {
            if let Token::Ident(n) = &w[1].node {
                if *n == name {
                    return Some(w[1].span);
                }
            }
        }
    }
    None
}

const KEYWORDS: &[&str] = &[
    "let", "fn", "if", "else", "type", "case", "impl", "do", "import", "pub", "as", "true", "false",
];
const BUILTINS: &[&str] = &[
    "int_to_float", "float_to_int", "int_to_long", "long_to_int", "int_to_string",
    "float_to_string", "string_to_int", "print", "read_line", "read_file", "write_file", "now",
    "pure", "delay", "run", "tcp_listen", "sock_port", "accept", "sock_read", "sock_write",
    "sock_close", "tcp_connect", "read_file_async",
];

// 補完候補: キーワード＋組込＋このファイルのトップレベル名（値・型・構築子）。
// 位置非依存（エディタがプレフィックスで絞る）。構文エラーでもキーワード/組込は返す。
pub fn completions(source: &str) -> Vec<String> {
    let mut out: Vec<String> = KEYWORDS.iter().chain(BUILTINS).map(|s| s.to_string()).collect();
    if let Ok(tokens) = tokenize(source) {
        if let Ok(program) = Parser::new(tokens).parse_program() {
            for s in &program {
                match s {
                    Stmt::Let(name, _, _) => out.push(name.clone()),
                    Stmt::Type(def) => {
                        out.push(def.name.clone());
                        for v in &def.variants {
                            out.push(v.name.clone());
                        }
                    }
                    Stmt::Expr(_) | Stmt::Impl(_) => {}
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::line_col_to_offset;

    #[test]
    fn module_members_は公開メンバーとドキュメントを集める() {
        let src = "// オプション型\n\
                   pub type Option(a) { Some(a); None }\n\
                   // 中身に関数適用\n\
                   pub let map: fn(Option(a), fn(a) -> b) -> Option(b) = fn(o, f) { o }\n\
                   let hidden = 1\n";
        let ms = module_members(src);
        let names: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert!(names.contains(&"Option")); // pub 型
        assert!(names.contains(&"Some")); // 構築子
        assert!(names.contains(&"None"));
        assert!(names.contains(&"map")); // pub 値
        assert!(!names.contains(&"hidden")); // 非 pub は出ない
        let map_m = ms.iter().find(|m| m.label == "map").unwrap();
        assert_eq!(map_m.doc.as_deref(), Some("中身に関数適用"));
        assert!(map_m.detail.as_deref().unwrap().contains("Option(a)"));
        let some_m = ms.iter().find(|m| m.label == "Some").unwrap();
        assert_eq!(some_m.kind, MemberKind::Constructor);
        assert_eq!(some_m.detail.as_deref(), Some("Some(a)"));
    }

    #[test]
    fn module_qualifier_path_を引く() {
        let src = "import std/option\nimport std/list as l\n0\n";
        assert_eq!(
            module_qualifier_path(src, "option"),
            Some(vec!["std".to_string(), "option".to_string()])
        );
        assert_eq!(
            module_qualifier_path(src, "l"),
            Some(vec!["std".to_string(), "list".to_string()])
        );
        assert_eq!(module_qualifier_path(src, "nope"), None);
    }

    #[test]
    fn type_at_変数() {
        let src = "let n = 1 + 2\nn";
        // 2 行目の n（line 1, col 0）の型は Int。
        let off = line_col_to_offset(src, 1, 0);
        assert_eq!(type_at(src, off), Some("Int".to_string()));
    }

    #[test]
    fn type_at_多相関数() {
        let src = "let id = fn(x) { x }\nid";
        let off = line_col_to_offset(src, 1, 0);
        assert_eq!(type_at(src, off), Some("fn(a) -> a".to_string()));
    }

    #[test]
    fn definition_at_トップレベル() {
        let src = "let double = fn(n: Int) -> Int { n * 2 }\ndouble(21)";
        // 2 行目の double（使用箇所）→ 1 行目の定義 double の span。
        let use_off = line_col_to_offset(src, 1, 0);
        let def = definition_at(src, use_off).expect("定義が見つかる");
        // 定義 "double" は 1 行目 col 4 から。
        assert_eq!(def.start, line_col_to_offset(src, 0, 4));
    }

    #[test]
    fn completions_にトップレベル名と組込() {
        let c = completions("let foo = 1\ntype Color { Red; Green }");
        assert!(c.contains(&"foo".to_string()));
        assert!(c.contains(&"Color".to_string()));
        assert!(c.contains(&"Red".to_string()));
        assert!(c.contains(&"print".to_string())); // 組込
        assert!(c.contains(&"case".to_string())); // キーワード
    }
}
