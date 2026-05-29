// awa の Language Server。まずは診断（型/構文エラー）だけを提供する。
// コンパイラは awa-core を再利用: check_source でエラーを取り、span を LSP の Range に変換する。
// 通信は LSP（stdio・JSON-RPC）。エディタがこのバイナリを起動して話す。
use std::error::Error;

use awa_core::check_source;
use awa_core::span::offset_to_line_col;
use lsp_server::{Connection, Message, Notification};
use lsp_types::notification::{Notification as _, PublishDiagnostics};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    Position, PublishDiagnosticsParams, Range, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url,
};

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    // stdio で LSP 接続を張る。
    let (connection, io_threads) = Connection::stdio();
    // テキスト同期は FULL（変更のたびに全文を受け取る）。診断のみなのでこれで十分。
    let capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    })?;
    connection.initialize(capabilities)?;
    // main_loop は Connection を所有して消費する。戻ると Connection が drop され、
    // sender が閉じて writer スレッドが終わる → io_threads.join() が返れる
    //（借用のままだと join がハングする lsp-server の定番の罠）。
    main_loop(connection)?;
    io_threads.join()?;
    Ok(())
}

fn main_loop(conn: Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                // hover/補完/定義ジャンプは未対応（診断のみ）。
            }
            Message::Notification(not) => match not.method.as_str() {
                "textDocument/didOpen" => {
                    let p: DidOpenTextDocumentParams = serde_json::from_value(not.params)?;
                    publish(&conn, p.text_document.uri, &p.text_document.text);
                }
                "textDocument/didChange" => {
                    let p: DidChangeTextDocumentParams = serde_json::from_value(not.params)?;
                    // FULL 同期なので最後の変更が全文。
                    if let Some(change) = p.content_changes.into_iter().last() {
                        publish(&conn, p.text_document.uri, &change.text);
                    }
                }
                _ => {}
            },
            Message::Response(_) => {}
        }
    }
    Ok(())
}

// テキストを検査して診断を publish する。
fn publish(conn: &Connection, uri: Url, text: &str) {
    let diagnostics = diagnostics_for(text);
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    let not = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    let _ = conn.sender.send(Message::Notification(not));
}

// awa-core の check_source を呼び、エラーがあれば 1 個の Diagnostic にする
//（awa は最初のエラーで止まるので診断は 0 個か 1 個）。
fn diagnostics_for(text: &str) -> Vec<Diagnostic> {
    match check_source(text) {
        Ok(()) => Vec::new(),
        Err(e) => {
            let range = match e.span {
                Some(s) => {
                    let (sl, sc) = offset_to_line_col(text, s.start);
                    let (el, ec) = offset_to_line_col(text, s.end);
                    Range::new(Position::new(sl, sc), Position::new(el, ec))
                }
                // 位置不明なら先頭を指す。
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

    #[test]
    fn 正しいプログラムは診断なし() {
        assert!(diagnostics_for("let f = fn(x: Int) -> Int { x + 1 }\nf(2)").is_empty());
    }

    #[test]
    fn 型エラーは診断1個() {
        let d = diagnostics_for("true + 1");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        // 位置（true + 1 は 0:0 から）。
        assert_eq!(d[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn 複数行のエラー位置() {
        // 2 行目の未定義変数 nope を指す（0 始まり: line 1）。
        let d = diagnostics_for("let x = 1\nnope");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range.start.line, 1);
    }

    #[test]
    fn didopenで診断がpublishされる() {
        use std::time::Duration;
        // メモリ接続でループを駆動し、エラーのある文書を開いて診断が返るか確認。
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
        drop(client); // チャネルを閉じてループを終わらせる
        let _ = handle.join();
    }
}
