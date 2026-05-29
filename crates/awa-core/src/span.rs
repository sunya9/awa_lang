// ===== ソース位置（Span）=====
// ソース文字列上の範囲を char オフセットの半開区間 [start, end) で表す。
// lexer が char の添字でトークン化するのに合わせて char 単位（バイトでない）。
// 日本語混じりのソースでも桁ずれしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    // 2 つの span を包む最小の span（子ノードから親ノードの span を作るのに使う）。
    pub fn merge(a: Span, b: Span) -> Self {
        Span {
            start: a.start.min(b.start),
            end: a.end.max(b.end),
        }
    }
}

// char オフセットを 0 始まりの (行, 列) に変換する（LSP の Position 用）。
// 列は char 単位（LSP 既定は UTF-16 だが、BMP（日本語含む）では一致する）。
pub fn offset_to_line_col(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut col = 0u32;
    for (i, c) in source.chars().enumerate() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

// 値に span を添えた汎用ラッパ。トークン列に使う。
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}
