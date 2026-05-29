// awa の VSCode 拡張本体。awa-lsp（LSP サーバ）を起動して言語クライアントを繋ぐ。
// シンタックスハイライトは syntaxes/awa.tmLanguage.json が宣言的に担うのでここでは扱わない。
const { workspace, window } = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  // awa.serverPath（既定 "awa-lsp"）。リポジトリ内なら target/debug/awa-lsp を指すとよい。
  const serverPath = workspace.getConfiguration("awa").get("serverPath") || "awa-lsp";

  const serverOptions = {
    command: serverPath,
    transport: TransportKind.stdio,
  };
  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "awa" }],
  };

  client = new LanguageClient(
    "awa",
    "awa language server",
    serverOptions,
    clientOptions
  );
  client.start().catch((err) => {
    window.showErrorMessage(
      `awa-lsp を起動できません (${serverPath}): ${err}. ` +
        `cargo build -p awa-lsp してから awa.serverPath を target/debug/awa-lsp に設定してください。`
    );
  });
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
