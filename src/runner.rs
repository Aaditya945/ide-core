use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Serialize, Debug, Clone)]
pub struct RuntimeDiagnostic {
    pub line: usize,
    pub column: usize,
    pub severity: String,
    pub message: String,
}

pub struct PtySession {
    pub writer: Box<dyn Write + Send>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    pub _master: Box<dyn MasterPty + Send>,
}

pub struct ExecManager {
    next_id: u64,
    pub sessions: Arc<Mutex<HashMap<u64, PtySession>>>,
}

struct LangCmd {
    lang: &'static str,
    compile: Option<(&'static str, Vec<String>)>,
    run: (&'static str, Vec<String>),
}

impl ExecManager {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(
        &mut self,
        uri: &str,
        code: &str,
        tx_out: UnboundedSender<String>,
    ) -> Result<u64, String> {
        let file_name = Path::new(uri)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("snippet.txt");

        let run_dir = std::env::temp_dir().join("ide_core_runs");
        std::fs::create_dir_all(&run_dir).map_err(|e| format!("Temp dir error: {e}"))?;
        let file_path = run_dir.join(file_name);
        std::fs::write(&file_path, code).map_err(|e| format!("Write error: {e}"))?;

        let src = file_path.to_string_lossy().to_string();
        let bin_name = if cfg!(windows) { "out_bin.exe" } else { "out_bin" };
        let bin = run_dir.join(bin_name).to_string_lossy().to_string();
        let spec = build_spec(uri, &src, &bin)?;

        let session_id = self.next_id;
        self.next_id += 1;

        if let Some((comp_prog, comp_args)) = spec.compile {
            let out = Command::new(comp_prog)
                .args(&comp_args)
                .output()
                .await
                .map_err(|e| format!("Failed to start '{comp_prog}': {e}"))?;

            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let combined = if stderr.is_empty() { &stdout } else { &stderr };
                let diagnostics = parse_diagnostics(spec.lang, combined);
                let _ = tx_out.send(
                    json!({
                        "event": "exec/output",
                        "session_id": session_id,
                        "data": combined
                    })
                    .to_string(),
                );
                let _ = tx_out.send(
                    json!({
                        "event": "exec/exit",
                        "session_id": session_id,
                        "phase": "compile",
                        "exit_code": out.status.code().unwrap_or(-1),
                        "diagnostics": diagnostics
                    })
                    .to_string(),
                );
                return Ok(session_id);
            }
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("PTY error: {e}"))?;

        let mut cmd = CommandBuilder::new(spec.run.0);
        for arg in &spec.run.1 {
            cmd.arg(arg);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to launch '{}': {e}", spec.run.0))?;
        drop(pair.slave);

        let killer = child.clone_killer();
        let writer = pair.master.take_writer().map_err(|e| format!("PTY writer: {e}"))?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("PTY reader: {e}"))?;

        {
            let mut map = self.sessions.lock().unwrap();
            map.insert(
                session_id,
                PtySession {
                    writer,
                    killer,
                    _master: pair.master,
                },
            );
        }

        let sessions_ref = Arc::clone(&self.sessions);
        let lang = spec.lang;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            let mut full_log = String::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                        full_log.push_str(&chunk);
                        let _ = tx_out.send(
                            json!({
                                "event": "exec/output",
                                "session_id": session_id,
                                "data": chunk
                            })
                            .to_string(),
                        );
                    }
                }
            }

            let exit_code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let diagnostics = if exit_code == 0 {
                Vec::new()
            } else {
                parse_diagnostics(lang, &full_log)
            };

            sessions_ref.lock().unwrap().remove(&session_id);
            let _ = tx_out.send(
                json!({
                    "event": "exec/exit",
                    "session_id": session_id,
                    "phase": "runtime",
                    "exit_code": exit_code,
                    "diagnostics": diagnostics
                })
                .to_string(),
            );
        });

        Ok(session_id)
    }

    pub fn send_input(&self, session_id: u64, data: &str) -> Result<(), String> {
        let mut map = self.sessions.lock().unwrap();
        let session = map
            .get_mut(&session_id)
            .ok_or_else(|| format!("Session {session_id} not running"))?;
        session
            .writer
            .write_all(data.as_bytes())
            .and_then(|_| session.writer.flush())
            .map_err(|e| format!("Input write error: {e}"))
    }

    pub fn kill(&self, session_id: u64) -> Result<(), String> {
        let mut map = self.sessions.lock().unwrap();
        let mut session = map
            .remove(&session_id)
            .ok_or_else(|| format!("Session {session_id} not running"))?;
        session.killer.kill().map_err(|e| format!("Kill error: {e}"))
    }
}

fn build_spec(uri: &str, src: &str, bin: &str) -> Result<LangCmd, String> {
    let s = src.to_string();
    let b = bin.to_string();
    if uri.ends_with(".py") {
        Ok(LangCmd { lang: "python", compile: None, run: ("python", vec!["-u".into(), s]) })
    } else if uri.ends_with(".js") {
        Ok(LangCmd { lang: "javascript", compile: None, run: ("node", vec![s]) })
    } else if uri.ends_with(".ts") {
        Ok(LangCmd { lang: "typescript", compile: None, run: ("npx", vec!["tsx".into(), s]) })
    } else if uri.ends_with(".java") {
        Ok(LangCmd { lang: "java", compile: None, run: ("java", vec![s]) })
    } else if uri.ends_with(".go") {
        Ok(LangCmd { lang: "go", compile: None, run: ("go", vec!["run".into(), s]) })
    } else if uri.ends_with(".c") {
        Ok(LangCmd {
            lang: "c",
            compile: Some(("gcc", vec![s, "-o".into(), b.clone()])),
            run: (Box::leak(b.into_boxed_str()), vec![]),
        })
    } else if uri.ends_with(".cpp") || uri.ends_with(".cc") {
        Ok(LangCmd {
            lang: "cpp",
            compile: Some(("g++", vec![s, "-o".into(), b.clone()])),
            run: (Box::leak(b.into_boxed_str()), vec![]),
        })
    } else if uri.ends_with(".rs") {
        Ok(LangCmd {
            lang: "rust",
            compile: Some(("rustc", vec![s, "-o".into(), b.clone()])),
            run: (Box::leak(b.into_boxed_str()), vec![]),
        })
    } else {
        Err(format!("No runner configured for {uri}"))
    }
}

fn parse_diagnostics(lang: &str, text: &str) -> Vec<RuntimeDiagnostic> {
    let mut diags = Vec::new();
    static STD_RE: OnceLock<Regex> = OnceLock::new();
    let std_re = STD_RE.get_or_init(|| {
        Regex::new(r"(?m)^(?:[A-Za-z]:)?[^:\r\n]+:(\d+)(?::(\d+))?:\s*(?:fatal\s+)?(error|warning|note)(?:\[[^\]]+\])?:\s*(.+)$").unwrap()
    });

    for cap in std_re.captures_iter(text) {
        let line = cap[1].parse().unwrap_or(1);
        let column = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
        diags.push(RuntimeDiagnostic {
            line,
            column,
            severity: cap[3].to_string(),
            message: cap[4].trim().to_string(),
        });
    }
    if !diags.is_empty() {
        return diags;
    }

    if lang == "python" {
        static PY_RE: OnceLock<Regex> = OnceLock::new();
        let py_re = PY_RE.get_or_init(|| Regex::new(r#"File "[^"]+", line (\d+)"#).unwrap());
        if let Some(last_cap) = py_re.captures_iter(text).last() {
            let line = last_cap[1].parse().unwrap_or(1);
            let msg = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Python error")
                .trim()
                .to_string();
            diags.push(RuntimeDiagnostic {
                line,
                column: 1,
                severity: "error".into(),
                message: msg,
            });
        }
    }
    diags
}
