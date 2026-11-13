//! Code Mode: run JavaScript in Deno with `tools.*` calls back into the
//! session's tool registry.

use super::{Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PRELUDE: &str = r#"
const encoder = new TextEncoder();
const decoder = new TextDecoder();
const reader = Deno.stdin.readable.getReader();
let buffer = "";
async function line() {
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline >= 0) {
      const value = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      return value;
    }
    const { value, done } = await reader.read();
    if (done) return buffer;
    buffer += decoder.decode(value, { stream: true });
  }
}
async function send(value) {
  await Deno.stdout.write(encoder.encode(JSON.stringify(value) + "\n"));
}
const input = JSON.parse(await line());
let sequence = 0;
const tools = new Proxy({}, {
  get(_target, name) {
    if (typeof name !== "string") return undefined;
    return async (parameters = {}) => {
      const id = ++sequence;
      await send({ type: "call", id, name, input: parameters });
      const response = JSON.parse(await line());
      if (response.error) throw new Error(response.error);
      return response.result;
    };
  },
});
console.log = (...args) => send({ type: "log", message: args.map((value) => typeof value === "string" ? value : JSON.stringify(value)).join(" ") });
console.error = console.log;
try {
  const fn = new Function("tools", "state", "now", "runs", "return (async () => {\n" + input.code + "\n})()");
  const result = await fn(tools, input.state ?? null, input.now ?? Date.now(), input.runs ?? 0);
  await send({ type: "result", result });
} catch (error) {
  await send({ type: "error", message: error && error.stack ? error.stack : String(error) });
}
"#;

pub struct Execute;

#[async_trait]
impl Tool for Execute {
    fn name(&self) -> &str {
        "execute"
    }

    fn description(&self) -> String {
        "Run JavaScript in a confined Deno runtime. Call session tools as `await tools.name(input)` (for example `await tools.read({ path: \"src/main.rs\" })`). The program receives `state`, `now` and `runs`, and may use the standard JavaScript built-ins. `fetch` and file access are unavailable. Return a value; non-string values are JSON-encoded.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "JavaScript to execute. Use await tools.<name>(input) to call tools." },
                "state": { "description": "Optional state passed as the `state` global." },
                "now": { "type": "integer", "description": "Optional current time in epoch milliseconds." },
                "runs": { "type": "integer", "description": "Optional completed run count." }
            },
            "required": ["code"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| anyhow!("code is required"))?
            .to_string();
        let deno = crate::plugin::ensure_deno().await?;
        let script = crate::paths::get().tmp.join(format!("codemode-{}.ts", std::process::id()));
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, PRELUDE)?;
        let mut command = tokio::process::Command::new(deno);
        command
            .arg("run")
            .arg("--no-prompt")
            .arg("--quiet")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("codemode stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("codemode stdout unavailable"))?;
        let mut lines = BufReader::new(stdout).lines();
        let initial = json!({
            "code": code,
            "state": input.get("state").cloned().unwrap_or(Value::Null),
            "now": input.get("now").cloned().unwrap_or(json!(crate::util::now_ms())),
            "runs": input.get("runs").cloned().unwrap_or(json!(0)),
        });
        stdin.write_all(format!("{initial}\n").as_bytes()).await?;
        stdin.flush().await?;
        let registry = crate::tools::registry(&ctx.app, &ctx.agent, &ctx.model).await;
        let mut logs = Vec::new();
        let mut calls = 0u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let message = tokio::time::timeout_at(deadline, lines.next_line())
                .await
                .map_err(|_| anyhow!("Code Mode timed out"))??
                .ok_or_else(|| anyhow!("Code Mode exited without a result"))?;
            let Ok(value) = serde_json::from_str::<Value>(&message) else { continue };
            match value.get("type").and_then(Value::as_str) {
                Some("log") => {
                    if let Some(message) = value.get("message").and_then(Value::as_str) {
                        logs.push(message.to_string());
                    }
                }
                Some("call") => {
                    calls += 1;
                    if calls > 100 {
                        return Err(anyhow!("Code Mode exceeded its tool call limit"));
                    }
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    let name = value.get("name").and_then(Value::as_str).unwrap_or_default();
                    let parameters = value.get("input").cloned().unwrap_or(Value::Null);
                    let tool = registry.iter().find(|tool| tool.name() == name && name != "execute");
                    let response = match tool {
                        Some(tool) => match tool.execute(parameters, ctx).await {
                            Ok(output) => json!({
                                "id": id,
                                "result": {
                                    "output": output.output,
                                    "title": output.title,
                                    "metadata": output.metadata,
                                }
                            }),
                            Err(error) => json!({ "id": id, "error": format!("{error:#}") }),
                        },
                        None => json!({ "id": id, "error": format!("unknown tool: {name}") }),
                    };
                    stdin.write_all(format!("{response}\n").as_bytes()).await?;
                    stdin.flush().await?;
                }
                Some("result") => {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    let text = match result {
                        Value::String(text) => text,
                        value => serde_json::to_string_pretty(&value).unwrap_or_default(),
                    };
                    let mut text = crate::util::truncate_chars(&text, 32 * 1024);
                    if !logs.is_empty() {
                        text.push_str("\n\nconsole:\n");
                        text.push_str(&logs.join("\n"));
                    }
                    let _ = child.kill().await;
                    return Ok(Output::text(text, "Code Mode").meta(json!({
                        "toolCalls": calls,
                        "logs": logs,
                    })));
                }
                Some("error") => {
                    let _ = child.kill().await;
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Code Mode failed")
                        .to_string();
                    return Err(anyhow!(message));
                }
                _ => {}
            }
        }
    }
}
