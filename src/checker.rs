use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Serialize, Debug, Clone)]
pub struct CompilerDiagnostic {
    pub source: &'static str,
    pub tool: String,
    pub code: Option<String>,
    pub severity: String,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct CheckResult {
    pub language: String,
    pub tool: String,
    pub available: bool,
    pub diagnostics: Vec<CompilerDiagnostic>,
    pub raw_output: String,
}

#[derive(Serialize, Debug)]
pub struct ToolchainStatus {
    pub language: &'static str,
    pub runner_or_compiler: &'static str,
    pub compiler_ready: bool,
    pub lsp_server: &'static str,
    pub lsp_ready: bool,
}

pub fn env_check() -> Vec<ToolchainStatus> {
    let entries = [
        ("python", "python", "pyright-langserver"),
        ("javascript", "node", "typescript-language-server"),
        ("typescript", "tsc", "typescript-language-server"),
        ("java", "javac", "jdtls"),
        ("c", "gcc", "clangd"),
        ("cpp", "g++", "clangd"),
        ("csharp", "dotnet", "csharp-ls"),
        ("rust", "rustc", "rust-analyzer"),
        ("go", "go", "gopls"),
        ("bash", "bash", "bash-language-server"),
    ];
    entries
        .into_iter()
        .map(|(language, comp, lsp)| ToolchainStatus {
            language,
            runner_or_compiler: comp,
            compiler_ready: command_exists(comp),
            lsp_server: lsp,
            lsp_ready: command_exists(lsp),
        })
        .collect()
}

fn command_exists(prog: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else { return false };
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    for dir in std::env::split_paths(&path_var) {
        for ext in exts {
            if dir.join(format!("{prog}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

pub async fn check_buffer(uri: &str, code: &str) -> Result<CheckResult, String> {
    let file_name = Path::new(uri)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("snippet.txt");

    let check_dir = std::env::temp_dir().join("ide_core_checks");
    std::fs::create_dir_all(&check_dir).map_err(|e| format!("Temp dir error: {e}"))?;
    let file_path = check_dir.join(file_name);
    std::fs::write(&file_path, code).map_err(|e| format!("Write error: {e}"))?;

    let src = file_path.to_string_lossy().to_string();
    let dir_str = check_dir.to_string_lossy().to_string();
    let meta_out = check_dir.join("out.rmeta").to_string_lossy().to_string();

    let (lang, prog, args): (&str, &str, Vec<String>) = if uri.ends_with(".py") {
        ("python", "python", vec!["-m".into(), "py_compile".into(), src])
    } else if uri.ends_with(".js") || uri.ends_with(".jsx") {
        ("javascript", "node", vec!["--check".into(), src])
    } else if uri.ends_with(".ts") || uri.ends_with(".tsx") {
        ("typescript", "tsc", vec!["--noEmit".into(), "--pretty".into(), "false".into(), src])
    } else if uri.ends_with(".java") {
        ("java", "javac", vec!["-Xlint:all".into(), "-d".into(), dir_str, src])
    } else if uri.ends_with(".c") {
        ("c", "gcc", vec!["-fsyntax-only".into(), "-Wall".into(), "-Wextra".into(), src])
    } else if uri.ends_with(".cpp") || uri.ends_with(".cc") {
        ("cpp", "g++", vec!["-fsyntax-only".into(), "-Wall".into(), "-Wextra".into(), src])
    } else if uri.ends_with(".rs") {
        (
            "rust",
            "rustc",
            vec!["--error-format=json".into(), "--emit=metadata".into(), "-o".into(), meta_out, src],
        )
    } else if uri.ends_with(".go") {
        ("go", "go", vec!["vet".into(), src])
    } else if uri.ends_with(".sh") || uri.ends_with(".bash") {
        ("bash", "bash", vec!["-n".into(), src])
    } else {
        return Err(format!("No compiler checker configured for {uri}"));
    };

    let mut cmd = Command::new(prog);
    cmd.args(&args);

    let output = match timeout(Duration::from_secs(10), cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => {
            return Ok(CheckResult {
                language: lang.into(),
                tool: prog.into(),
                available: false,
                diagnostics: Vec::new(),
                raw_output: format!("'{prog}' not found in PATH"),
            });
        }
        Err(_) => return Err(format!("'{prog}' check timed out")),
    };

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let combined = if stderr.trim().is_empty() { &stdout } else { &stderr };

    let diagnostics = if output.status.success() && combined.trim().is_empty() {
        Vec::new()
    } else if lang == "rust" {
        parse_rustc_json(prog, combined)
    } else {
        parse_text_diagnostics(lang, prog, combined)
    };

    Ok(CheckResult {
        language: lang.into(),
        tool: prog.into(),
        available: true,
        diagnostics,
        raw_output: combined.to_string(),
    })
}

fn parse_rustc_json(tool: &str, text: &str) -> Vec<CompilerDiagnostic> {
    let mut diags = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let level = v.get("level").and_then(|s| s.as_str()).unwrap_or("");
        if level != "error" && level != "warning" {
            continue;
        }
        let message = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Rust compiler diagnostic")
            .to_string();
        let code = v
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .map(str::to_string);

        let spans_empty = v
            .get("spans")
            .and_then(|s| s.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if spans_empty && code.is_none() && message.starts_with("aborting due to") {
            continue;
        }

        let span = v
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|sp| sp.get("is_primary").and_then(|b| b.as_bool()) == Some(true))
                    .or_else(|| arr.first())
            });

        let (line_s, col_s, line_e, col_e, span_label) = if let Some(sp) = span {
            (
                sp.get("line_start").and_then(|n| n.as_u64()).unwrap_or(1) as usize,
                sp.get("column_start").and_then(|n| n.as_u64()).unwrap_or(1) as usize,
                sp.get("line_end").and_then(|n| n.as_u64()).unwrap_or(1) as usize,
                sp.get("column_end").and_then(|n| n.as_u64()).unwrap_or(1) as usize,
                sp.get("label").and_then(|l| l.as_str()).map(str::to_string),
            )
        } else {
            (1, 1, 1, 1, None)
        };

        let child_hint = v
            .get("children")
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|ch| {
                    let lvl = ch.get("level").and_then(|s| s.as_str())?;
                    if lvl == "help" || lvl == "note" {
                        ch.get("message").and_then(|m| m.as_str()).map(str::to_string)
                    } else {
                        None
                    }
                })
            });

        diags.push(CompilerDiagnostic {
            source: "compiler",
            tool: tool.into(),
            code,
            severity: level.into(),
            line: line_s,
            column: col_s,
            end_line: line_e,
            end_column: col_e,
            message,
            hint: child_hint.or(span_label),
        });
    }
    diags
}

fn parse_text_diagnostics(lang: &str, tool: &str, text: &str) -> Vec<CompilerDiagnostic> {
    let mut diags = Vec::new();
    static STD_RE: OnceLock<Regex> = OnceLock::new();
    let std_re = STD_RE.get_or_init(|| {
        Regex::new(r"(?m)^(?:[A-Za-z]:)?[^:\r\n]+:(\d+)(?::(\d+))?:\s*(?:fatal\s+)?(error|warning|note):\s*(.+)$").unwrap()
    });

    for cap in std_re.captures_iter(text) {
        let line = cap[1].parse().unwrap_or(1);
        let col = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
        diags.push(CompilerDiagnostic {
            source: "compiler",
            tool: tool.into(),
            code: None,
            severity: cap[3].to_string(),
            line,
            column: col,
            end_line: line,
            end_column: col + 1,
            message: cap[4].trim().to_string(),
            hint: None,
        });
    }

    if !diags.is_empty() {
        return diags;
    }

    if lang == "python" {
        static PY_RE: OnceLock<Regex> = OnceLock::new();
        let py_re = PY_RE.get_or_init(|| Regex::new(r#"File "[^"]+", line (\d+)"#).unwrap());
        if let Some(cap) = py_re.captures_iter(text).last() {
            let line = cap[1].parse().unwrap_or(1);
            let msg = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Python syntax error")
                .trim()
                .to_string();
            diags.push(CompilerDiagnostic {
                source: "compiler",
                tool: tool.into(),
                code: None,
                severity: "error".into(),
                line,
                column: 1,
                end_line: line,
                end_column: 1,
                message: msg,
                hint: None,
            });
        }
    } else if lang == "javascript" {
        static JS_RE: OnceLock<Regex> = OnceLock::new();
        let js_re = JS_RE.get_or_init(|| Regex::new(r"(?m)^(?:[A-Za-z]:)?[^:\r\n]+\.(?:js|jsx|mjs):(\d+)").unwrap());
        if let Some(cap) = js_re.captures(text) {
            let line = cap[1].parse().unwrap_or(1);
            let msg = text
                .lines()
                .find(|l| l.contains("Error:"))
                .unwrap_or("JavaScript syntax error")
                .trim()
                .to_string();
            diags.push(CompilerDiagnostic {
                source: "compiler",
                tool: tool.into(),
                code: None,
                severity: "error".into(),
                line,
                column: 1,
                end_line: line,
                end_column: 1,
                message: msg,
                hint: None,
            });
        }
    }

    diags
}
