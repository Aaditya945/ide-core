use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

struct LspInstance {
    stdin: ChildStdin,
    child: Child,
    next_id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    opened_docs: HashMap<String, i32>,
    last_used: Instant,
}

pub struct LspManager {
    servers: HashMap<String, LspInstance>,
}

impl LspManager {
    pub fn new() -> Self {
        Self { servers: HashMap::new() }
    }

    fn server_cmd(lang: &str) -> Option<(&'static str, Vec<&'static str>)> {
        match lang {
            "python" => Some(("pyright-langserver", vec!["--stdio"])),
            "javascript" | "typescript" | "tsx" => Some(("typescript-language-server", vec!["--stdio"])),
            "rust" => Some(("rust-analyzer", vec![])),
            "c" | "cpp" => Some(("clangd", vec![])),
            "go" => Some(("gopls", vec![])),
            "java" => Some(("jdtls", vec![])),
            "csharp" => Some(("csharp-ls", vec![])),
            "bash" => Some(("bash-language-server", vec!["start"])),
            _ => None,
        }
    }

    pub fn detect_lang(uri: &str) -> Option<&'static str> {
        if uri.ends_with(".py") { Some("python") }
        else if uri.ends_with(".js") || uri.ends_with(".jsx") { Some("javascript") }
        else if uri.ends_with(".ts") || uri.ends_with(".tsx") { Some("typescript") }
        else if uri.ends_with(".rs") { Some("rust") }
        else if uri.ends_with(".c") || uri.ends_with(".h") { Some("c") }
        else if uri.ends_with(".cpp") || uri.ends_with(".cc") { Some("cpp") }
        else if uri.ends_with(".go") { Some("go") }
        else if uri.ends_with(".java") { Some("java") }
        else if uri.ends_with(".cs") { Some("csharp") }
        else if uri.ends_with(".sh") || uri.ends_with(".bash") { Some("bash") }
        else { None }
    }

    async fn ensure_server(
        &mut self,
        lang: &str,
        root_uri: &str,
        tx_out: UnboundedSender<String>,
    ) -> Result<&mut LspInstance, String> {
        if !self.servers.contains_key(lang) {
            let (prog, args) = Self::server_cmd(lang)
                .ok_or_else(|| format!("No LSP configured for {lang}"))?;

            let mut child = Command::new(prog)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Failed to start LSP '{prog}': {e} (install it in PATH)"))?;

            let stdin = child.stdin.take().ok_or("No LSP stdin")?;
            let stdout = child.stdout.take().ok_or("No LSP stdout")?;
            let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let pending_ref = Arc::clone(&pending);
            let lang_name = lang.to_string();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut content_len: usize = 0;
                    let mut header = String::new();
                    loop {
                        header.clear();
                        match reader.read_line(&mut header).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {
                                let trimmed = header.trim();
                                if trimmed.is_empty() {
                                    break;
                                }
                                if let Some(val) = trimmed.strip_prefix("Content-Length:") {
                                    content_len = val.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if content_len == 0 {
                        continue;
                    }
                    let mut body = vec![0u8; content_len];
                    if reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    if let Ok(msg) = serde_json::from_slice::<Value>(&body) {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            if let Some(tx) = pending_ref.lock().unwrap().remove(&id) {
                                let _ = tx.send(msg);
                            }
                        } else if msg.get("method").and_then(|m| m.as_str())
                            == Some("textDocument/publishDiagnostics")
                        {
                            let _ = tx_out.send(
                                json!({
                                    "event": "lsp/diagnostics",
                                    "language": lang_name,
                                    "params": msg.get("params")
                                })
                                .to_string(),
                            );
                        }
                    }
                }
            });

            let mut inst = LspInstance {
                stdin,
                child,
                next_id: 1,
                pending,
                opened_docs: HashMap::new(),
                last_used: Instant::now(),
            };

            let _ = Self::send_request(
                &mut inst,
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {
                        "textDocument": {
                            "completion": { "completionItem": { "snippetSupport": true } },
                            "hover": { "contentFormat": ["markdown", "plaintext"] },
                            "codeAction": { "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix", "refactor"] } } }
                        }
                    }
                }),
            )
            .await?;
            Self::send_notification(&mut inst, "initialized", json!({})).await?;
            self.servers.insert(lang.to_string(), inst);
        }

        let inst = self.servers.get_mut(lang).unwrap();
        inst.last_used = Instant::now();
        Ok(inst)
    }

    async fn write_lsp(stdin: &mut ChildStdin, payload: &Value) -> Result<(), String> {
        let body = serde_json::to_string(payload).unwrap();
        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        stdin.write_all(msg.as_bytes()).await.map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn send_notification(inst: &mut LspInstance, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        Self::write_lsp(&mut inst.stdin, &payload).await
    }

    async fn send_request(inst: &mut LspInstance, method: &str, params: Value) -> Result<Value, String> {
        let id = inst.next_id;
        inst.next_id += 1;
        let (tx, rx) = oneshot::channel();
        inst.pending.lock().unwrap().insert(id, tx);

        let payload = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        Self::write_lsp(&mut inst.stdin, &payload).await?;

        let resp = rx.await.map_err(|_| "LSP channel closed".to_string())?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn sync_doc(
        &mut self,
        uri: &str,
        text: &str,
        tx_out: UnboundedSender<String>,
    ) -> Result<String, String> {
        let lang = Self::detect_lang(uri).ok_or("Unsupported language")?;
        let doc_uri = to_file_uri(uri);
        let root_uri = to_file_uri(".");
        let inst = self.ensure_server(lang, &root_uri, tx_out).await?;

        if let Some(ver) = inst.opened_docs.get_mut(&doc_uri) {
            *ver += 1;
            let v = *ver;
            Self::send_notification(
                inst,
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": doc_uri, "version": v },
                    "contentChanges": [{ "text": text }]
                }),
            )
            .await?;
        } else {
            inst.opened_docs.insert(doc_uri.clone(), 1);
            Self::send_notification(
                inst,
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": doc_uri,
                        "languageId": lang,
                        "version": 1,
                        "text": text
                    }
                }),
            )
            .await?;
        }
        Ok(lang.to_string())
    }

    pub async fn query_lsp(
        &mut self,
        uri: &str,
        method: &str,
        extra_params: Value,
        tx_out: UnboundedSender<String>,
    ) -> Result<Value, String> {
        let lang = Self::detect_lang(uri).ok_or("Unsupported language")?;
        let doc_uri = to_file_uri(uri);
        let root_uri = to_file_uri(".");
        let inst = self.ensure_server(lang, &root_uri, tx_out).await?;

        let mut params = extra_params;
        if let Some(obj) = params.as_object_mut() {
            obj.insert("textDocument".into(), json!({ "uri": doc_uri }));
        }
        Self::send_request(inst, method, params).await
    }

    pub async fn stop(&mut self, lang: &str) -> Result<(), String> {
        if let Some(mut inst) = self.servers.remove(lang) {
            let _ = inst.child.kill().await;
            Ok(())
        } else {
            Err(format!("LSP for {lang} not running"))
        }
    }

    pub async fn reap_idle(&mut self, max_idle: Duration, tx_out: &UnboundedSender<String>) {
        let expired: Vec<String> = self
            .servers
            .iter()
            .filter(|(_, inst)| inst.last_used.elapsed() > max_idle)
            .map(|(k, _)| k.clone())
            .collect();

        for lang in expired {
            if let Some(mut inst) = self.servers.remove(&lang) {
                let _ = inst.child.kill().await;
                let _ = tx_out.send(
                    json!({
                        "event": "lsp/stopped",
                        "language": lang,
                        "reason": "idle_timeout"
                    })
                    .to_string(),
                );
            }
        }
    }
}

fn to_file_uri(path: &str) -> String {
    if path.starts_with("file://") {
        return path.to_string();
    }
    let abs = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap().join(path));
    let s = abs.to_string_lossy().replace('\\', "/");
    let clean = s.strip_prefix("//?/").unwrap_or(&s);
    if clean.starts_with('/') {
        format!("file://{clean}")
    } else {
        format!("file:///{clean}")
    }
}
