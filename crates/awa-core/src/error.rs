use std::fmt;

use crate::span::Span;

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
    UnterminatedString,        // 字句解析: " が閉じられていない
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
    // ----- ADT / パターンマッチ（Phase 3） -----
    UnknownType(String),        // 型式が参照する型名が未定義
    UnknownConstructor(String), // パターン等が参照する構築子が未定義
    NoMatch,                    // 評価: どのパターンにも当たらなかった（網羅性が防ぐ穴の保険）
    // ----- 網羅性・到達不能（Phase 3b） -----
    NonExhaustiveMatch { witness: String }, // case に覆えない値がある（witness=足りない例）
    UnreachablePattern(String),             // 上の腕に覆われて決して当たらない腕
    // or パターンの選択肢が束縛する変数の集合が食い違う
    OrPatternBindings,
    // 型注釈が推論された型より一般的すぎる（実際に持っていない型を主張している）
    AnnotationTooGeneral { inferred: String, annotation: String },
    // ----- メソッド（Phase 6） -----
    UnknownMethod { ty: String, method: String }, // その型に該当メソッドが無い
    MethodReceiverUnknown,                         // レシーバの型が未確定で解決できない
    // ----- 標準型（Phase 8） -----
    AmbiguousNumeric,    // 算術の数値型が定まらない（型注釈が必要）
    NotNumeric(String),  // 算術/比較の対象が数値型でない
    // ----- コード生成（Phase 9） -----
    Unsupported(String), // codegen がまだ対応していない構造/型
    // ----- モジュール（Phase 10） -----
    NotExported { module: String, name: String }, // 参照先がそのモジュールで pub 公開されていない
    UnknownModule(String),                        // 修飾アクセスの左がモジュールでない（未 import 等）
    PrivateMethod { ty: String, method: String }, // 非公開メソッドを他モジュールから呼んだ
    EntryConflict, // 入口 main があるのにトップレベルにむき出しの式がある
    // ----- 非同期（Phase 11） -----
    Deadlock,    // イベントループが空なのに run の Future が未解決
    // ----- OS 連携（Phase 12） -----
    Io(String),  // ファイル等の入出力エラー
}

// Display を実装すると `{}` で人間向けに表示できる（Debug の `{:?}` は開発者向け）。
// 将来 Phase 4 の「良質なエラーメッセージ」へ育てていく入口。
impl fmt::Display for AwaError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AwaError::UnexpectedChar(c) => write!(f, "予期しない文字: {}", c),
            AwaError::InvalidNumber(s) => write!(f, "数値として解釈できません: {}", s),
            AwaError::UnterminatedComment => write!(f, "コメント /* が閉じられていません"),
            AwaError::UnterminatedString => write!(f, "文字列 \" が閉じられていません"),
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
            AwaError::UnknownType(name) => write!(f, "未定義の型です: {}", name),
            AwaError::UnknownConstructor(name) => write!(f, "未定義の構築子です: {}", name),
            AwaError::NoMatch => write!(f, "どのパターンにも一致しませんでした"),
            AwaError::NonExhaustiveMatch { witness } => {
                write!(f, "case が網羅的でありません: {} に当たる腕がありません", witness)
            }
            AwaError::UnreachablePattern(pat) => {
                write!(f, "決して当たらないパターンです: {}", pat)
            }
            AwaError::OrPatternBindings => {
                write!(f, "or パターンの各選択肢は同じ変数を束縛する必要があります")
            }
            AwaError::AnnotationTooGeneral {
                inferred,
                annotation,
            } => write!(
                f,
                "型注釈が一般的すぎます: {} と注釈されていますが、実際の型は {} です",
                annotation, inferred
            ),
            AwaError::UnknownMethod { ty, method } => {
                write!(f, "型 {} にメソッド {} はありません", ty, method)
            }
            AwaError::MethodReceiverUnknown => write!(
                f,
                "レシーバの型が定まらずメソッドを解決できません（型注釈を付けてください）"
            ),
            AwaError::AmbiguousNumeric => {
                write!(f, "数値型が定まりません。型注釈を付けてください")
            }
            AwaError::NotNumeric(ty) => {
                write!(f, "型 {} は数値ではありません（算術/比較は Int/Long/Float のみ）", ty)
            }
            AwaError::Unsupported(what) => {
                write!(f, "コード生成は未対応です: {}", what)
            }
            AwaError::EntryConflict => {
                write!(
                    f,
                    "main があるときはトップレベルにむき出しの式を置けません（処理は main の中へ）"
                )
            }
            AwaError::NotExported { module, name } => {
                write!(f, "{} は {} で公開されていません（pub を付けてください）", name, module)
            }
            AwaError::UnknownModule(name) => {
                write!(f, "モジュールではありません（未 import の可能性）: {}", name)
            }
            AwaError::PrivateMethod { ty, method } => write!(
                f,
                "メソッド {}.{} は非公開です（定義側で pub fn か pub impl にしてください）",
                ty, method
            ),
            AwaError::Deadlock => write!(
                f,
                "イベントループがデッドロックしました（待機中の Future が解決されません）"
            ),
            AwaError::Io(msg) => write!(f, "入出力エラー: {}", msg),
        }
    }
}

// ===== 位置つきエラー =====
// AwaError（エラーの「種類」）に、発生したソース上の位置（span）を添えたもの。
// パイプライン（tokenize/parse/check/eval）はこれを返す。span は不明なら None。
#[derive(Debug, PartialEq)]
pub struct LocatedError {
    pub error: AwaError,
    pub span: Option<Span>,
}

impl LocatedError {
    pub fn at(error: AwaError, span: Span) -> Self {
        LocatedError {
            error,
            span: Some(span),
        }
    }
}

// span を持たない AwaError は ? で自動的に位置なし LocatedError に昇格する。
impl From<AwaError> for LocatedError {
    fn from(error: AwaError) -> Self {
        LocatedError { error, span: None }
    }
}

// エラーを人間向けに整形する。span があれば「行:列」とソース該当行＋キャレットを付ける。
pub fn report(err: &LocatedError, source: &str) -> String {
    report_at(&err.error, err.span, source, None)
}

// report の本体。エラー・位置・ソースを別々に受け取り、任意で `-->` 行にファイル名を出す。
// モジュール対応で「連結後の span を元ファイルのローカル span に直して報告する」用途に使う。
pub fn report_at(
    error: &AwaError,
    span: Option<Span>,
    source: &str,
    filename: Option<&str>,
) -> String {
    let Some(span) = span else {
        return format!("error: {}", error);
    };
    // char オフセットから行・列を求める（行は 1 始まり、列も 1 始まり）。
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, c) in source.chars().enumerate() {
        if i >= span.start {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    // 該当行のテキストを取り出す。
    let line_text = source.lines().nth(line - 1).unwrap_or("");
    // キャレット行: 列まで空白を詰めて span の長さ分 ^ を引く。
    let width = span.end.saturating_sub(span.start).max(1);
    let caret = format!("{}{}", " ".repeat(col - 1), "^".repeat(width));
    let loc = match filename {
        Some(f) => format!("{}:{}:{}", f, line, col),
        None => format!("{}:{}", line, col),
    };
    format!(
        "error: {}\n --> {}\n  |\n  | {}\n  | {}",
        error, loc, line_text, caret
    )
}
