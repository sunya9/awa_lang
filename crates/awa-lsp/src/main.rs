// awa の Language Server。診断（型/構文エラー）に加え、hover（型表示）・定義ジャンプ・補完を提供する。
// コンパイラは awa-core を再利用: 診断は check_source、位置クエリは query::{type_at, definition_at, completions}。
// 通信は LSP（stdio・JSON-RPC）。エディタがこのバイナリを起動して話す。
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;

use awa_core::query::{completions, definition_at, type_at};
use awa_core::span::{line_col_to_offset, offset_to_line_col};
use awa_core::{check_document, check_source, span::Span};
use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::notification::{Notification as _, PublishDiagnostics};
use lsp_types::{
    CompletionItem, CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, Location, MarkedString, OneOf, Position, PublishDiagnosticsParams,
    Range, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use serde_json::Value;

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions::default()),
        ..Default::default()
    })?;
    connection.initialize(capabilities)?;
    // main_loop は Connection を所有して消費する（戻ると drop され io_threads.join() が返れる）。
    main_loop(connection)?;
    io_threads.join()?;
    Ok(())
}

fn main_loop(conn: Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
    // 開いている文書（uri → 全文）。hover/定義/補完はこの内容に対して位置クエリする。
    let mut docs: HashMap<Url, String> = HashMap::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                let result = match req.method.as_str() {
                    "textDocument/hover" => handle_hover(&docs, req.params),
                    "textDocument/definition" => handle_definition(&docs, req.params),
                    "textDocument/completion" => handle_completion(&docs, req.params),
                    _ => Value::Null,
                };
                conn.sender
                    .send(Message::Response(Response::new_ok(req.id, result)))?;
            }
            Message::Notification(not) => match not.method.as_str() {
                "textDocument/didOpen" => {
                    let p: DidOpenTextDocumentParams = serde_json::from_value(not.params)?;
                    let uri = p.text_document.uri;
                    docs.insert(uri.clone(), p.text_document.text.clone());
                    publish(&conn, uri, &p.text_document.text);
                }
                "textDocument/didChange" => {
                    let p: DidChangeTextDocumentParams = serde_json::from_value(not.params)?;
                    let uri = p.text_document.uri;
                    // FULL 同期なので最後の変更が全文。
                    if let Some(change) = p.content_changes.into_iter().last() {
                        docs.insert(uri.clone(), change.text.clone());
                        publish(&conn, uri, &change.text);
                    }
                }
                _ => {}
            },
            Message::Response(_) => {}
        }
    }
    Ok(())
}

// span（char オフセット）を LSP の Range に変換。
fn span_to_range(text: &str, span: Span) -> Range {
    let (sl, sc) = offset_to_line_col(text, span.start);
    let (el, ec) = offset_to_line_col(text, span.end);
    Range::new(Position::new(sl, sc), Position::new(el, ec))
}

// hover: カーソル位置の式の型を表示。
fn handle_hover(docs: &HashMap<Url, String>, params: Value) -> Value {
    let p: HoverParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(_) => return Value::Null,
    };
    let pos = p.text_document_position_params.position;
    let Some(text) = docs.get(&p.text_document_position_params.text_document.uri) else {
        return Value::Null;
    };
    let off = line_col_to_offset(text, pos.line, pos.character);
    match type_at(text, off) {
        Some(ty) => serde_json::to_value(Hover {
            contents: HoverContents::Scalar(MarkedString::String(ty)),
            range: None,
        })
        .unwrap_or(Value::Null),
        None => Value::Null,
    }
}

// 定義ジャンプ: カーソル位置の名前のトップレベル定義へ。
fn handle_definition(docs: &HashMap<Url, String>, params: Value) -> Value {
    let p: GotoDefinitionParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(_) => return Value::Null,
    };
    let uri = p.text_document_position_params.text_document.uri;
    let pos = p.text_document_position_params.position;
    let Some(text) = docs.get(&uri) else {
        return Value::Null;
    };
    let off = line_col_to_offset(text, pos.line, pos.character);
    match definition_at(text, off) {
        Some(span) => {
            let loc = Location::new(uri, span_to_range(text, span));
            serde_json::to_value(GotoDefinitionResponse::Scalar(loc)).unwrap_or(Value::Null)
        }
        None => Value::Null,
    }
}

// 補完: キーワード＋組込＋トップレベル名。
fn handle_completion(docs: &HashMap<Url, String>, params: Value) -> Value {
    let p: CompletionParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(_) => return Value::Null,
    };
    let uri = &p.text_document_position.text_document.uri;
    let Some(text) = docs.get(uri) else {
        return Value::Null;
    };
    let pos = p.text_document_position.position;
    // カーソル行が `import ...` なら、import 可能なモジュール名を提案する。
    // そうでなければ通常の補完（キーワード/組込/トップレベル名）。
    let labels = if is_import_line(text, pos.line) {
        import_completions(uri)
    } else {
        completions(text)
    };
    let items: Vec<CompletionItem> = labels
        .into_iter()
        .map(|label| CompletionItem {
            label,
            ..Default::default()
        })
        .collect();
    serde_json::to_value(CompletionResponse::Array(items)).unwrap_or(Value::Null)
}

// 指定行（0 始まり）が import 文か（先頭の空白を除いて "import" で始まる）。
fn is_import_line(text: &str, line: u32) -> bool {
    text.lines()
        .nth(line as usize)
        .map(|l| l.trim_start().starts_with("import"))
        .unwrap_or(false)
}

// import 可能なモジュールパス: 焼き込み済みの std/* と、文書と同じディレクトリの兄弟 .awa。
fn import_completions(uri: &Url) -> Vec<String> {
    let mut paths: Vec<String> = awa_core::stdlib::STDLIB
        .iter()
        .map(|(p, _)| p.to_string())
        .collect();
    if let Ok(path) = uri.to_file_path() {
        let self_stem = path.file_stem().map(|s| s.to_os_string());
        if let Some(dir) = path.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("awa")
                        && p.file_stem().map(|s| s.to_os_string()) != self_stem
                    {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            paths.push(stem.to_string());
                        }
                    }
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

// テキストを検査して診断を publish する。
fn publish(conn: &Connection, uri: Url, text: &str) {
    let params = PublishDiagnosticsParams {
        diagnostics: diagnostics_for(&uri, text),
        uri,
        version: None,
    };
    let not = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    let _ = conn.sender.send(Message::Notification(not));
}

// 文書を検査して診断にする（awa は最初のエラーで止まるので最大 1 個）。
// ファイル URI なら import を解決して検査（check_document＝バッファを上書きしてロード）。
// それ以外（untitled 等）はファイルパスが無いので単一ファイル検査にフォールバックする。
fn diagnostics_for(uri: &Url, text: &str) -> Vec<Diagnostic> {
    let result = match uri.to_file_path() {
        Ok(path) => {
            let root = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // stdlib=None＝焼き込みの std を使う（cwd/環境に非依存）。
            check_document(&root, None, &name, text)
        }
        Err(_) => check_source(text),
    };
    match result {
        Ok(()) => Vec::new(),
        Err(e) => {
            let range = match e.span {
                Some(s) => span_to_range(text, s),
                None => Range::new(Position::new(0, 0), Position::new(0, 1)),
            };
            vec![Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("awa".to_string()),
                message: format!("{}", e.error),
                ..Default::default()
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{TextDocumentItem, notification::DidOpenTextDocument};

    fn uri() -> Url {
        Url::parse("file:///t.awa").unwrap()
    }

    #[test]
    fn 正しいプログラムは診断なし() {
        assert!(diagnostics_for(&uri(), "let f = fn(x: Int) -> Int { x + 1 }\nf(2)").is_empty());
    }

    #[test]
    fn 型エラーは診断1個() {
        let d = diagnostics_for(&uri(), "true + 1");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn 複数行のエラー位置() {
        let d = diagnostics_for(&uri(), "let x = 1\nnope");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range.start.line, 1);
    }

    #[test]
    fn import_std_は未定義にならない() {
        // 焼き込みの std を解決するので、io.println は未定義扱いされない。
        let src = "import std/io\nio.println(\"hi\")\n";
        assert!(
            diagnostics_for(&uri(), src).is_empty(),
            "import 解決後は診断なしのはず"
        );
    }

    #[test]
    fn import行ではモジュールが補完される() {
        assert!(is_import_line("import std/io", 0));
        assert!(is_import_line("  import foo", 0));
        assert!(!is_import_line("let x = 1", 0));
        // std/* は焼き込みから常に提案できる。
        let comp = import_completions(&uri());
        assert!(comp.iter().any(|c| c == "std/io"));
        assert!(comp.iter().any(|c| c == "std/list"));
    }

    #[test]
    fn didopenで診断がpublishされる() {
        use std::time::Duration;
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || {
            let _ = main_loop(server);
        });
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: Url::parse("file:///t.awa").unwrap(),
                language_id: "awa".to_string(),
                version: 1,
                text: "true + 1".to_string(),
            },
        };
        let not = Notification::new(DidOpenTextDocument::METHOD.to_string(), params);
        client.sender.send(Message::Notification(not)).unwrap();
        let msg = client
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("診断が返る");
        match msg {
            Message::Notification(n) => {
                assert_eq!(n.method, PublishDiagnostics::METHOD);
                let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                assert_eq!(p.diagnostics.len(), 1);
            }
            other => panic!("通知を期待: {:?}", other),
        }
        drop(client);
        let _ = handle.join();
    }

    #[test]
    fn hover補完定義のスモーク() {
        // ドキュメントを直接入れて各ハンドラを呼ぶ（位置クエリの結線確認）。
        let mut docs = HashMap::new();
        let uri = Url::parse("file:///t.awa").unwrap();
        let src = "let double = fn(n: Int) -> Int { n * 2 }\ndouble(21)".to_string();
        docs.insert(uri.clone(), src.clone());

        // hover: 2 行目の double（line 1, col 0）の型は fn(Int) -> Int。
        let hover_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 0 }
        });
        let h = handle_hover(&docs, hover_params);
        assert!(h.to_string().contains("fn(Int) -> Int"), "hover: {}", h);

        // definition: double の定義（1 行目）へ。
        let def_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 2 }
        });
        let d = handle_definition(&docs, def_params);
        assert!(d.to_string().contains("\"line\":0"), "definition: {}", d);

        // completion: double と組込/キーワードが候補に。
        let comp_params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 0 }
        });
        let c = handle_completion(&docs, comp_params);
        assert!(c.to_string().contains("double"), "completion: {}", c);
    }
}
