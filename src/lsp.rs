//! Minimal config-driven LSP diagnostics.
//!
//! A server is started per check, initialized, opened on the edited file,
//! queried for `textDocument/publishDiagnostics`, then shut down. This keeps the
//! runtime small while still surfacing the diagnostics editors would show.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

struct Server {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl Server {
    async fn start(command: &[String], env: &serde_json::Map<String, Value>, cwd: &Path) -> Result<Self> {
        let (prog, args) = command.split_first().ok_or_else(|| anyhow!("lsp command is empty"))?;
        let mut process = Command::new(prog);
        process
            .args(args)
            .envs(env.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x0800_0000);
        }
        let mut child = process.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("lsp stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("lsp stdout unavailable"))?;
        Ok(Server { child, stdin, stdout: BufReader::new(stdout) })
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(&body).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<Value> {
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).await?;
            if read == 0 {
                return Err(anyhow!("lsp server closed"));
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        if length == 0 {
            return Err(anyhow!("lsp message had no length"));
        }
        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    async fn stop(mut self) {
        let _ = self.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": null })).await;
        let _ = self.send(&json!({ "jsonrpc": "2.0", "method": "exit", "params": null })).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

fn config_for<'a>(config: &'a Value, path: &Path) -> Option<&'a Value> {
    let extension = path.extension().and_then(|value| value.to_str())?;
    let extension = format!(".{extension}");
    let servers = config.as_object()?;
    for (_, server) in servers {
        if server.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let matches = server
            .get("extensions")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).any(|item| item == extension))
            .unwrap_or(false);
        if matches {
            return Some(server);
        }
    }
    None
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()).unwrap_or_default() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "sh" | "bash" => "shellscript",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        _ => "plaintext",
    }
}

fn file_uri(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    url::Url::from_file_path(&path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.to_string_lossy().replace('\\', "/")))
}

/// Format diagnostics for a tool result, or an empty string when no server
/// handles the file.
pub async fn diagnostics_text(app: &crate::app::App, path: &Path) -> String {
    if app.config().lsp.is_none() {
        return String::new();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return String::new(),
    };
    match diagnostics(app, path, &text).await {
        Ok(items) if items.is_empty() => String::new(),
        Ok(items) => {
            let mut output = String::from("\n\nLSP diagnostics:\n");
            for item in items {
                output.push_str(&format!(
                    "{}:{}: {} {}\n",
                    item.get("line").and_then(Value::as_u64).unwrap_or(0) + 1,
                    item.get("column").and_then(Value::as_u64).unwrap_or(0) + 1,
                    item.get("severity").and_then(Value::as_str).unwrap_or("error"),
                    item.get("message").and_then(Value::as_str).unwrap_or("")
                ));
            }
            output
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_a_server_by_extension() {
        let config = json!({
            "rust": { "command": ["rust-analyzer"], "extensions": [".rs"] },
            "typescript": { "command": ["typescript-language-server", "--stdio"], "extensions": [".ts", ".tsx"] }
        });
        assert!(config_for(&config, Path::new("src/main.rs")).is_some());
        assert!(config_for(&config, Path::new("src/main.ts")).is_some());
        assert!(config_for(&config, Path::new("README.md")).is_none());
    }

    #[test]
    fn skips_disabled_servers() {
        let config = json!({
            "rust": { "disabled": true, "command": ["rust-analyzer"], "extensions": [".rs"] }
        });
        assert!(config_for(&config, Path::new("main.rs")).is_none());
    }
}

pub async fn diagnostics(app: &crate::app::App, path: &Path, text: &str) -> Result<Vec<Value>> {
    let app_config = app.config();
    let Some(config) = app_config.lsp.as_ref().and_then(|config| config_for(config, path)) else {
        return Ok(Vec::new());
    };
    let command = config
        .get("command")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();
    let env = config.get("env").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut server = Server::start(&command, &env, &app.worktree).await?;
    let root = app.worktree.to_string_lossy().to_string();
    server
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": file_uri(&app.worktree),
                "rootPath": root,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {
                            "relatedInformation": false,
                            "versionSupport": false
                        }
                    }
                },
                "initializationOptions": config.get("initialization").cloned().unwrap_or(Value::Null),
            }
        }))
        .await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), server.read()).await;
    server.send(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })).await?;
    let uri = file_uri(path);
    server
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id(path),
                    "version": 1,
                    "text": text,
                }
            }
        }))
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut result = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let message = match tokio::time::timeout_at(deadline, server.read()).await {
            Ok(Ok(message)) => message,
            _ => break,
        };
        if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
            continue;
        }
        let Some(diagnostics) = message.pointer("/params/diagnostics").and_then(Value::as_array) else {
            continue;
        };
        for diagnostic in diagnostics {
            result.push(json!({
                "line": diagnostic.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(0),
                "column": diagnostic.pointer("/range/start/character").and_then(Value::as_u64).unwrap_or(0),
                "severity": match diagnostic.get("severity").and_then(Value::as_u64) {
                    Some(1) => "error",
                    Some(2) => "warning",
                    Some(3) => "info",
                    _ => "hint",
                },
                "message": diagnostic.get("message").and_then(Value::as_str).unwrap_or(""),
                "source": diagnostic.get("source").and_then(Value::as_str),
            }));
        }
        break;
    }
    server.stop().await;
    Ok(result)
}
