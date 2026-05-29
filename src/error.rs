use std::fmt;

// ===== エラー型 =====
// 言語全体で使うエラーを 1 つの enum にまとめる。
// これ自体が ADT（代数的データ型）の練習であり、Phase 3 で本格化する
// パターンマッチ／網羅性チェックの土台になる。
// ヴァリアントを増やすと、match で対応漏れがコンパイルエラーになる。
#[derive(Debug, PartialEq)]
pub enum AwaError {
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
    // ----- 型推論（Phase 2） -----
    InfiniteType,          // occurs check 失敗: a = a -> b のような無限型
    CannotUnify {
        // 単一化失敗: 噛み合わない2つの型（Num と Bool など）
        left: String,
        right: String,
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
            AwaError::InfiniteType => write!(f, "型エラー: 無限型は作れません"),
            AwaError::CannotUnify { left, right } => {
                write!(f, "型エラー: {} と {} は一致しません", left, right)
            }
        }
    }
}
