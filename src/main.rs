mod buffer;
mod checker;
mod lsp;
mod protocol;
mod runner;
mod syntax;

use buffer::BufferStore;
use lsp::LspManager;
use protocol::{Request, RpcEnvelope, RpcResponse};
use runner::ExecManager;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use syntax::SyntaxEngine;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut store = BufferStore::new();
    let mut syntax = SyntaxEngine::new();
    let mut exec = ExecManager::new();
    let lsp = Arc::new(Mutex::new(LspManager::new()));

    let (tx_out, mut rx_out) = mpsc::unbounded_channel::<String>();
    let writer_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(mut msg) = rx_out.recv().await {
            msg.push('\n');
            if stdout.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let lsp_reaper = Arc::clone(&lsp);
    let tx_reaper = tx_out.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            lsp_reaper
                .lock()
                .await
                .reap_idle(Duration::from_secs(300), &tx_reaper)
                .await;
        }
    });

    let mut lines = BufReader::new(io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<RpcEnvelope>(&line) {
            Ok(env) => handle_request(env, &mut store, &mut syntax, &mut exec, &lsp, &tx_out).await,
            Err(e) => RpcResponse {
                id: 0,
                result: None,
                error: Some(format!("Invalid JSON-RPC: {e}")),
            },
        };
        let _ = tx_out.send(serde_json::to_string(&resp).unwrap());
    }

    while !exec.sessions.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(tx_out);
    let _ = writer_handle.await;
    Ok(())
}

async fn handle_request(
    env: RpcEnvelope,
    store: &mut BufferStore,
    syntax: &mut SyntaxEngine,
    exec: &mut ExecManager,
    lsp: &Arc<Mutex<LspManager>>,
    tx_out: &mpsc::UnboundedSender<String>,
) -> RpcResponse {
    match env.req {
        Request::OpenBuffer { uri, text } => {
            syntax.invalidate(&uri);
            store.open(uri, &text);
            RpcResponse { id: env.id, result: Some(json!({ "status": "opened" })), error: None }
        }
        Request::EditBuffer { uri, start_char, end_char, new_text } => {
            match store.edit(&uri, start_char, end_char, &new_text) {
                Ok(delta) => {
                    syntax.apply_edit(&uri, &delta);
                    RpcResponse { id: env.id, result: Some(json!({ "lines": delta.lines })), error: None }
                }
                Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
            }
        }
        Request::BufferInfo { uri } => match store.stats(&uri) {
            Some((lines, chars)) => RpcResponse {
                id: env.id,
                result: Some(json!({ "lines": lines, "chars": chars })),
                error: None,
            },
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::Analyze { uri } => match store.get_text(&uri) {
            Some(text) => match syntax.analyze(&uri, &text) {
                Ok(analysis) => RpcResponse { id: env.id, result: Some(json!(analysis)), error: None },
                Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
            },
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::Check { uri } => match store.get_text(&uri) {
            Some(text) => match checker::check_buffer(&uri, &text).await {
                Ok(res) => RpcResponse { id: env.id, result: Some(json!(res)), error: None },
                Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
            },
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::Diagnostics { uri } => match store.get_text(&uri) {
            Some(text) => {
                let syn = syntax.analyze(&uri, &text);
                let comp = checker::check_buffer(&uri, &text).await;
                let mut merged = Vec::new();
                let mut symbols = Vec::new();
                let mut language = String::new();
                let mut compiler_available = false;

                if let Ok(s) = syn {
                    language = s.language;
                    symbols = s.symbols;
                    for e in s.errors {
                        merged.push(json!({
                            "source": e.source,
                            "severity": e.severity,
                            "line": e.line,
                            "column": e.column,
                            "end_line": e.end_line,
                            "end_column": e.end_column,
                            "message": e.message,
                            "code": serde_json::Value::Null,
                            "hint": serde_json::Value::Null
                        }));
                    }
                }
                if let Ok(c) = comp {
                    if language.is_empty() {
                        language = c.language;
                    }
                    compiler_available = c.available;
                    for d in c.diagnostics {
                        merged.push(json!({
                            "source": d.source,
                            "severity": d.severity,
                            "line": d.line,
                            "column": d.column,
                            "end_line": d.end_line,
                            "end_column": d.end_column,
                            "message": d.message,
                            "code": d.code,
                            "hint": d.hint
                        }));
                    }
                }
                RpcResponse {
                    id: env.id,
                    result: Some(json!({
                        "language": language,
                        "compiler_available": compiler_available,
                        "diagnostics": merged,
                        "symbols": symbols
                    })),
                    error: None,
                }
            }
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::EnvCheck => RpcResponse {
            id: env.id,
            result: Some(json!({ "toolchains": checker::env_check() })),
            error: None,
        },
        Request::ExecStart { uri } => match store.get_text(&uri) {
            Some(text) => match exec.start(&uri, &text, tx_out.clone()).await {
                Ok(session_id) => RpcResponse {
                    id: env.id,
                    result: Some(json!({ "session_id": session_id })),
                    error: None,
                },
                Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
            },
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::ExecInput { session_id, data } => match exec.send_input(session_id, &data) {
            Ok(_) => RpcResponse { id: env.id, result: Some(json!({ "sent": true })), error: None },
            Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
        },
        Request::ExecKill { session_id } => match exec.kill(session_id) {
            Ok(_) => RpcResponse { id: env.id, result: Some(json!({ "killed": true })), error: None },
            Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
        },
        Request::LspSync { uri } => match store.get_text(&uri) {
            Some(text) => match lsp.lock().await.sync_doc(&uri, &text, tx_out.clone()).await {
                Ok(lang) => RpcResponse { id: env.id, result: Some(json!({ "synced": lang })), error: None },
                Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
            },
            None => RpcResponse { id: env.id, result: None, error: Some("File not found".into()) },
        },
        Request::LspComplete { uri, line, character } => {
            lsp_call(env.id, lsp, &uri, "textDocument/completion", json!({ "position": { "line": line, "character": character } }), tx_out).await
        }
        Request::LspDefinition { uri, line, character } => {
            lsp_call(env.id, lsp, &uri, "textDocument/definition", json!({ "position": { "line": line, "character": character } }), tx_out).await
        }
        Request::LspReferences { uri, line, character } => {
            lsp_call(env.id, lsp, &uri, "textDocument/references", json!({ "position": { "line": line, "character": character }, "context": { "includeDeclaration": true } }), tx_out).await
        }
        Request::LspHover { uri, line, character } => {
            lsp_call(env.id, lsp, &uri, "textDocument/hover", json!({ "position": { "line": line, "character": character } }), tx_out).await
        }
        Request::LspCodeAction { uri, line, character, end_line, end_character } => {
            lsp_call(env.id, lsp, &uri, "textDocument/codeAction", json!({ "range": { "start": { "line": line, "character": character }, "end": { "line": end_line, "character": end_character } }, "context": { "diagnostics": [] } }), tx_out).await
        }
        Request::LspRename { uri, line, character, new_name } => {
            lsp_call(env.id, lsp, &uri, "textDocument/rename", json!({ "position": { "line": line, "character": character }, "newName": new_name }), tx_out).await
        }
        Request::LspFormat { uri } => {
            lsp_call(env.id, lsp, &uri, "textDocument/formatting", json!({ "options": { "tabSize": 4, "insertSpaces": true } }), tx_out).await
        }
        Request::LspStop { language } => match lsp.lock().await.stop(&language).await {
            Ok(_) => RpcResponse { id: env.id, result: Some(json!({ "stopped": language })), error: None },
            Err(e) => RpcResponse { id: env.id, result: None, error: Some(e) },
        },
    }
}

async fn lsp_call(
    id: u64,
    lsp: &Arc<Mutex<LspManager>>,
    uri: &str,
    method: &str,
    params: serde_json::Value,
    tx_out: &mpsc::UnboundedSender<String>,
) -> RpcResponse {
    match lsp
        .lock()
        .await
        .query_lsp(uri, method, params, tx_out.clone())
        .await
    {
        Ok(res) => RpcResponse { id, result: Some(res), error: None },
        Err(e) => RpcResponse { id, result: None, error: Some(e) },
    }
}
