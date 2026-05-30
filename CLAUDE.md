# awa_lang

Rust 製・Gleam 寄りの関数型言語（コマンド名 `awa`）。フロントエンド（lexer/parser/HM 型推論/網羅性）＋2バックエンド: tree-walking インタプリタ（`awa run` ＝足場）と **LLVM テキスト IR codegen（`awa build` → clang -O2 ＝本命）**。GC は所有権規約の参照カウント（awa は純粋＝循環不能なので RC が完全な回収器）。

> このプロジェクトの詳細な進捗・設計経緯は、開発者のローカルメモリ（`~/.claude/.../memory/`）にも蓄積している（マシン依存で他環境には来ない）。このファイルは他PCでも作業を再開できるよう、普遍的な規約・現在地・次の一手だけを残す。

## 作業の流儀（重要）

- **大きなソース編集は1箇所ずつ、ユニークな old_string で**。`replace_all` や同一ファイルへの同時多数編集は避ける（過去に main.rs が約10万行に膨張・破損した実績あり）。大きな構造変更は Write で全置換する方が安全。
- 破損したら `git show <commit>:path > path` で復元（`git restore` はサンドボックスで拒否されることがある）。
- 機能の区切りでこまめにコミット（**conventional commits・英文**）。
- **テストは `timeout 200 cargo test --workspace` で回す**（時間制限なしだと孤立 cargo がビルドロックを掴んでハングした実績）。`--workspace` 必須（root の `default-members` が awa-cli のみ＝付け忘れると awa-lsp がステイルになる罠）。
- author 名義は `xyz <mugen.xyz@gmail.com>` に統一済み。

## 構成（Cargo workspace）

- `crates/awa-core` — lib。lexer/parser/infer/types/eval/codegen/resolve/error/span/token/value/match_check/fmt/query/stdlib、`runtime.c`（C ランタイム）。
- `crates/awa-cli` — bin `awa`（run/build/emit/fmt/init/REPL）。
- `crates/awa-lsp` — bin `awa-lsp`（stdio LSP。診断/hover/定義/補完）。awa-core を再利用。
- `editors/vscode` — VSCode 拡張（TextMate 文法＋LSP クライアント）。
- `std/` — 標準ライブラリ（awa ソース）。コンパイラに `include_str!` で焼き込み（`stdlib.rs`）＋ディスク `./std` 優先。**std 編集後は焼き込み反映に `cargo build` が要る**（cwd に std/ があればディスク優先で即反映）。
- `examples/` — 統合テストを兼ねる（`include_str!`）。

## 言語設計の方向性

- **構文は Scala 寄り（ASI・ブロック・`${}` 補間・`|>`・`do`）、型は Gleam/ML 寄り（暗黙 HM・自動一般化）**。この2軸は独立。
- **Gleam を北極星**＝言語は小さく保つ。**型クラスは最終手段**（pipe も do も型クラス無しで実装済み＝do は `.flatMap`/`.map` メソッドへ脱糖）。
- `impl` は trait/型クラスではなく**具体型への inherent メソッド付与・静的ディスパッチ**（Rust の inherent impl 相当）。チェッカが `recv.m()` を `Call(Var("型頭#m"), [recv,..])` へ lower。
- **`if` は廃止済み**（条件分岐は `case c { true -> a; false -> b }`。網羅性検査に一本化）。
- **ガード節は入れない**（網羅性検査の旨味を削ぐため）。
- 「もう学習プロジェクトではない」＝production グレードの設計・外部クレート依存・本格最適化を遠慮なく入れてよい。ただしインタプリタは足場・**LLVM コンパイラが本筋**という優先順位は不変。

## 現在地（実装済みの主な機能）

- 型: Int/Long/Float/String/Bool/Unit、ADT＋パターンマッチ＋網羅性、レコード（名前付きフィールド・更新・パターン）、タプル（ヘテロ任意長）、配列（不変・永続 `Array(a)`、内部 transient ビルダーで map/filter 等 O(n)）。
- 演算子: 四則＋ `%`、比較、`&& || `、ビット演算 `& | ^ << >> ~`、複合型の構造的 `==`/`!=`（native も実装済み）。
- 構文: let 多相、型注釈＋subsumption、or パターン、`|>`、`do` 記法、メソッド（pub impl / pub fn 二段可視性）、三連引用符＋`${}` 補間、16/8/2進リテラル。
- モジュール（Gleam 風 1ファイル=1モジュール、`pub`、修飾名、別名、循環検出、越境網羅性、per-file エラー位置）。
- 相互再帰（トップレベルは両バックエンド可。ブロック内はインタプリタのみ＝native は RC 循環のため明示エラー）。
- TCO（native は clang -O2 の sibling-call、インタプリタはトランポリン）。
- 非同期（Node 風シングルスレッド・イベントループ）: Future/`pure`/`delay`/`run`/flatMap/map、論理時間＋実時間タイマー、kqueue ソケット真非同期、スレッドプールでファイル疑似非同期、`all`。両バックエンド exit 0（リークなし）。
- 標準ライブラリ: io/option/result/list/array/int/float/string/pair/dict/math/json/http。
- ツール: `awa fmt`（Wadler 整形・幅100/2スペース・コメント保持）、`awa.toml`（path/git 依存・build/ 出力・`awa init`/`awa update` は未）、LSP（診断/hover/定義/補完・`.` 後のモジュールメンバ補完）、VSCode 拡張。
- エラーは `--> 行:列` ＋ソース行＋キャレット表示（span 基盤）。

詳しい各機能のコミット単位の経緯はローカルメモリ側にある。

## 次にやりたいこと（候補）

- **レコードのフィールドアクセスを行多相で注釈不要に**（現状 `o.x` は `o` の具体型注釈が必要）。
- 順序比較 `<` 等を複合型へ（現状スカラのみ）。
- 論理右シフト `>>>`（現状 `>>` は算術 ashr のみ）。
- 配列 push/set 単発の O(log n) 化（RRB-tree。ただし at が O(log n) に悪化するトレードオフ要検討）。
- 注釈の subsumption を lambda 引数/戻りにも拡張、双方向型推論（注釈の重複削減）。
- async v2: `race`、非同期ファイル書き込み。
- パッケージ: 推移的依存解決、`awa update`（lock 更新）、version/semver、tarball/レジストリ、private repo 認証。
- HTTP: keep-alive（接続再利用）、JSON サロゲートペア。
- std: set。`pub opaque type`（構築子を隠す）。
