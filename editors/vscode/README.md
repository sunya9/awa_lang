# awa — VSCode 拡張

awa 言語のシンタックスハイライトと Language Server（診断）を VSCode に提供する。
monorepo 同居（Rust ワークスペースとは独立。Cargo は `editors/` を無視する）。

## 構成
- `syntaxes/awa.tmLanguage.json` … TextMate 文法（ハイライト。npm 不要・インストールだけで効く）
- `language-configuration.json` … コメント・括弧の対応
- `src/extension.js` … `awa-lsp` を起動して LSP クライアントを繋ぐ（診断）

## 使い方（開発・お試し）
1. LSP サーバをビルド: リポジトリルートで
   ```sh
   cargo build -p awa-lsp        # → target/debug/awa-lsp
   ```
2. 拡張の依存を入れる（LSP クライアント用）:
   ```sh
   cd editors/vscode && npm install
   ```
3. VSCode で `editors/vscode` を開き、F5（Extension Development Host が起動）。
4. 拡張機能設定 `awa.serverPath` を `awa-lsp` バイナリの絶対パス
   （例 `/path/to/awa_lang/target/debug/awa-lsp`）にする。
5. `*.awa` を開くと色付き＋型/構文エラーが波線で出る。

シンタックスハイライトだけなら 1〜2 は不要（文法は宣言だけで動く）。
