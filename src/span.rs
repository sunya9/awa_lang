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
