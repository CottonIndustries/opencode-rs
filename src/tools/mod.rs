//! Built-in tools and the per-request registry.

pub mod artifact;
pub mod codemode;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod mcp_resource;
pub mod patch;
pub mod question;
pub mod read;
pub mod schedule;
pub mod shell;
pub mod skill;
pub mod subagent;
pub mod todo;
pub mod webfetch;
pub mod websearch;
pub mod write;

use crate::agent::Agent;
use crate::app::{App, Handle};
use crate::models::Model;
use crate::provider::ToolSpec;
use crate::session::FilePart;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Output {
    pub output: String,
    pub title: String,
    pub metadata: Value,
    pub attachments: Vec<FilePart>,
    /// Extra synthetic context appended after the result (e.g. newly discovered AGENTS.md).
    pub context: Option<String>,
}

impl Output {
    pub fn text(output: impl Into<String>, title: impl Into<String>) -> Self {
        Output { output: output.into(), title: title.into(), metadata: Value::Null, ..Default::default() }
    }
    pub fn meta(mut self, m: Value) -> Self {
        self.metadata = m;
        self
    }
}

pub struct Context {
    pub app: Arc<App>,
    pub handle: Arc<Handle>,
    pub agent: Agent,
    pub model: Model,
    pub session_id: String,
    /// id of the assistant message that requested the tool
    pub message_id: String,
    /// id of the tool call being executed
    pub call_id: String,
    pub cancel: CancellationToken,
}

impl Context {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = p.trim();
        let p = if p.is_empty() || p == "undefined" || p == "null" { "." } else { p };
        crate::util::resolve(&self.app.directory, p)
    }

    pub fn display(&self, p: &Path) -> String {
        crate::util::relative(&self.app.directory, p)
    }

    pub async fn ask(&self, action: &str, resources: &[String], save: &[String], title: String, metadata: Value) -> Result<()> {
        self.app.authorize(&self.handle, &self.agent, Some(&self.call_id), action, resources, save, title, metadata).await
    }

    /// Require `external_directory` permission for paths outside the project.
    pub async fn authorize_external(&self, path: &Path) -> Result<()> {
        let inside = crate::util::contains(&self.app.worktree, path)
            || crate::util::contains(&self.app.directory, path)
            || crate::util::contains(&crate::paths::get().tmp, path);
        if inside {
            return Ok(());
        }
        let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.to_path_buf()) };
        let pattern = format!("{}{}*", dir.display(), std::path::MAIN_SEPARATOR);
        self.ask(
            "external_directory",
            &[path.display().to_string()],
            &[pattern],
            format!("Access path outside the project: {}", path.display()),
            json!({ "filepath": path.display().to_string(), "parentDir": dir.display().to_string() }),
        )
        .await
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output>;
}

/// Helpers for reading typed arguments.
pub fn arg_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}
pub fn req_str<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    arg_str(v, k).ok_or_else(|| anyhow!("The {k} parameter is required and must be a string"))
}
pub fn arg_u64(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)).or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}
pub fn arg_bool(v: &Value, k: &str) -> Option<bool> {
    v.get(k).and_then(|x| x.as_bool().or_else(|| x.as_str().map(|s| s == "true")))
}

/// Truncate large tool output, saving the full text to a file (head preview).
pub fn truncate_output(text: &str) -> (String, bool) {
    let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return (text.to_string(), false);
    }
    let mut kept = Vec::new();
    let mut bytes = 0;
    for l in lines.iter().take(MAX_LINES) {
        let add = l.len() + if kept.is_empty() { 0 } else { 1 };
        if bytes + add > MAX_BYTES {
            break;
        }
        kept.push(*l);
        bytes += add;
    }
    let dir = crate::paths::get().tool_output();
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{}.txt", crate::util::id("tool")));
    let _ = std::fs::write(&file, text);
    let range = if kept.is_empty() { "0 lines".to_string() } else { format!("lines 1-{}", kept.len()) };
    (format!("{}\n\n[showing {range} of {}; full output saved to {}]", kept.join("\n"), lines.len(), file.display()), true)
}

pub fn spec(t: &dyn Tool) -> ToolSpec {
    ToolSpec { name: t.name().to_string(), description: t.description(), schema: t.schema() }
}

/// Tool metadata shown to plugins before transformation and model filtering.
pub fn descriptors(config: &crate::config::Config) -> Vec<(String, String, Value)> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(shell::Shell::new()),
        Arc::new(read::Read),
        Arc::new(write::Write),
        Arc::new(edit::Edit),
        Arc::new(patch::Patch),
        Arc::new(glob::Glob),
        Arc::new(grep::Grep),
        Arc::new(webfetch::WebFetch),
    ];
    if config.experimental.todo {
        tools.push(Arc::new(todo::TodoWrite));
    }
    // Include websearch unconditionally in the plugin-facing catalog; whether
    // it is active depends on configured or plugin-provided integrations.
    tools.push(Arc::new(websearch::WebSearch));
    tools.push(Arc::new(skill::SkillTool));
    let mut out: Vec<(String, String, Value)> = tools
        .iter()
        .map(|tool| (tool.name().to_string(), tool.description(), tool.schema()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The tool action used for permission rules.
pub fn permission_action(name: &str) -> &str {
    match name {
        "write" | "edit" | "patch" => "edit",
        other => other,
    }
}

/// A tool contributed by a plugin. Execution and permission checks happen in
/// the Deno host; Rust only publishes the model-facing definition.
pub struct PluginTool {
    pub id: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub permission: Option<String>,
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        if let Some(permission) = &self.permission {
            ctx.ask(
                permission,
                &[self.name.clone()],
                &[self.name.clone()],
                format!("Run plugin tool: {}", self.name),
                json!({ "tool": self.name, "input": input.clone() }),
            )
            .await?;
        }
        let context = json!({
            "sessionID": ctx.session_id,
            "agent": ctx.agent.id,
            "messageID": ctx.message_id,
            "id": ctx.call_id,
            "directory": ctx.app.directory.to_string_lossy(),
            "worktree": ctx.app.worktree.to_string_lossy(),
        });
        let result = ctx.app.plugins.execute_tool(&self.id, &ctx.call_id, input, context).await?;
        let output = result.get("output").and_then(Value::as_str).unwrap_or_default().to_string();
        let title = result.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
        let metadata = result.get("metadata").cloned().unwrap_or(Value::Null);
        let mut attachments = Vec::new();
        for attachment in result.get("attachments").and_then(Value::as_array).into_iter().flatten() {
            let mime = attachment.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
            let url = attachment.get("url").and_then(Value::as_str).unwrap_or_default();
            let data = url.split_once("base64,").map(|(_, data)| data.to_string()).unwrap_or_default();
            if !data.is_empty() {
                attachments.push(FilePart {
                    mime,
                    data,
                    name: attachment.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                });
            }
        }
        Ok(Output { output, title, metadata, attachments, context: None })
    }
}

/// Wraps a builtin tool with description/schema overrides from a plugin.
struct MetaTool {
    inner: Arc<dyn Tool>,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for MetaTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        self.inner.execute(input, ctx).await
    }
}

/// Tools available to this agent and model.
pub async fn registry(app: &Arc<App>, agent: &Agent, model: &Model) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(shell::Shell::new()),
        Arc::new(read::Read),
        Arc::new(write::Write),
        Arc::new(edit::Edit),
        Arc::new(patch::Patch),
        Arc::new(glob::Glob),
        Arc::new(grep::Grep),
        Arc::new(webfetch::WebFetch),
        Arc::new(artifact::Create),
        Arc::new(artifact::Read),
        Arc::new(artifact::Update),
        Arc::new(artifact::Delete),
        Arc::new(question::QuestionTool),
    ];
    if websearch::configured(&app.config).is_some() || app.plugins.has_websearch() {
        tools.push(Arc::new(websearch::WebSearch));
    }
    if app.mcp.configured() > 0 {
        tools.push(Arc::new(mcp_resource::ListResources));
        tools.push(Arc::new(mcp_resource::ReadResource));
    }
    if app.config.experimental.todo {
        tools.push(Arc::new(todo::TodoWrite));
    }
    if app.config.experimental.codemode {
        tools.push(Arc::new(codemode::Execute));
    }
    if app.config.experimental.schedules {
        tools.push(Arc::new(schedule::Create));
        tools.push(Arc::new(schedule::List));
        tools.push(Arc::new(schedule::Pause));
        tools.push(Arc::new(schedule::Resume));
        tools.push(Arc::new(schedule::Delete));
    }
    if !app.skills().is_empty() {
        tools.push(Arc::new(skill::SkillTool));
    }
    let subagents = subagent::available(app, agent);
    if !subagents.is_empty() {
        tools.push(Arc::new(subagent::Subagent { agents: subagents }));
    }
    let builtin_names: std::collections::BTreeSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
    for t in app.mcp.tools().await {
        tools.push(t);
    }

    // Plugin transforms can hide builtins, rewrite their descriptions or
    // schemas, and add new executable tools.
    let descriptors = app.plugins.tool_list().await;
    if app.plugins.enabled() {
        let descriptors_by_name: std::collections::BTreeMap<String, crate::plugin::ToolDescriptor> =
            descriptors.iter().map(|d| (d.name.clone(), d.clone())).collect();
        let mut next: Vec<Arc<dyn Tool>> = Vec::new();
        for tool in tools {
            let name = tool.name().to_string();
            if builtin_names.contains(&name) && !descriptors_by_name.contains_key(&name) {
                continue;
            }
            if let Some(descriptor) = descriptors_by_name.get(&name).filter(|d| d.source == "builtin") {
                next.push(Arc::new(MetaTool {
                    inner: tool,
                    description: descriptor.description.clone(),
                    schema: descriptor.schema.clone(),
                }));
            } else {
                next.push(tool);
            }
        }
        for descriptor in descriptors.iter().filter(|d| d.source != "builtin") {
            if next.iter().any(|t| t.name() == descriptor.name) {
                continue;
            }
            next.push(Arc::new(PluginTool {
                id: descriptor.id.clone(),
                name: descriptor.name.clone(),
                description: descriptor.description.clone(),
                schema: descriptor.schema.clone(),
                permission: descriptor.permission.clone(),
            }));
        }
        tools = next;
    }

    // model-specific tool sets
    let id = format!("{} {}", model.id, model.family).to_lowercase();
    let gpt_patch = model.id.contains("gpt-") && !model.id.contains("oss") && !model.id.contains("gpt-4");
    tools.retain(|t| {
        let n = t.name();
        if gpt_patch && (n == "edit" || n == "write") {
            return false;
        }
        if !gpt_patch && n == "patch" {
            return false;
        }
        if (id.contains("gpt") || id.contains("claude")) && (n == "grep" || n == "glob") {
            return false;
        }
        true
    });

    // config and agent tool toggles + permission denies
    let mut rules = crate::permission::defaults();
    if let Some(p) = &app.config.permission {
        rules.extend(crate::permission::from_config(p));
    }
    rules.extend(agent.permissions.iter().cloned());
    tools.retain(|t| {
        let n = t.name();
        let toggled = |map: &std::collections::BTreeMap<String, bool>| {
            map.iter().filter(|(k, _)| crate::util::wildcard(k, n)).last().map(|(_, v)| *v)
        };
        if toggled(&app.config.tools) == Some(false) || toggled(&agent.tools) == Some(false) {
            return false;
        }
        !crate::permission::disabled(&rules, permission_action(n))
    });
    if !model.tool_call {
        tools.clear();
    }
    // upstream sends its tools sorted by name
    tools.sort_by(|a, b| a.name().cmp(b.name()));
    tools
}
