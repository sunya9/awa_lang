// 標準ライブラリのソースをコンパイラバイナリへ焼き込む（Gleam の prelude 相当）。
// これにより `import std/...` は cwd・環境変数・インストール先に一切依存せず、
// どこから awa を起動しても必ず解決できる。配布物は awa バイナリ単体で完結する。
//
// 注意: これは「コンパイラ」に std の *ソーステキスト* を埋めるだけで、
// `awa build` の出力実行ファイルには影響しない（出力には codegen が実際に
// 発行した IR のみが入る。総称関数は使用箇所ごとの単相化で、呼ばれなければ
// 発行されない＝自動で落ちる）。

// (モジュールパス, ソース)。パスは "std/io" のようなスラッシュ区切り。
// パスは crates/awa-core/src/ からの相対なので、`../../../std/` がリポジトリの std/。
pub const STDLIB: &[(&str, &str)] = &[
    ("std/io", include_str!("../../../std/io.awa")),
    ("std/option", include_str!("../../../std/option.awa")),
    ("std/result", include_str!("../../../std/result.awa")),
    ("std/list", include_str!("../../../std/list.awa")),
    ("std/int", include_str!("../../../std/int.awa")),
    ("std/float", include_str!("../../../std/float.awa")),
    ("std/string", include_str!("../../../std/string.awa")),
    ("std/pair", include_str!("../../../std/pair.awa")),
    ("std/dict", include_str!("../../../std/dict.awa")),
    ("std/math", include_str!("../../../std/math.awa")),
    ("std/json", include_str!("../../../std/json.awa")),
    ("std/http", include_str!("../../../std/http.awa")),
];

// 焼き込み済み標準ライブラリのソースを返す（無ければ None）。
pub fn embedded_source(segs: &[String]) -> Option<&'static str> {
    let key = segs.join("/");
    STDLIB.iter().find(|(p, _)| *p == key).map(|(_, s)| *s)
}
