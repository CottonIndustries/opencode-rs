//! Model Context Protocol client (stdio and streamable HTTP) exposing server tools.

use crate::config::McpConfig;
use crate::tools::{Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const PROTOCOL: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub enum Status {
    Connected { tools: usize },
    Disabled,
    Failed(String),
}

enum Transport {
    Stdio {
        stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        _child: tokio::process::Child,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        session: Mutex<Option<String>>,
    },
    Socket {
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    },
}

pub struct Client {
    name: String,
    transport: Transport,
    next: AtomicU64,
    timeout: std::time::Duration,
    protocol: String,
}

impl Client {
    pub fn socket(
        name: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        timeout: std::time::Duration,
    ) -> Self {
        Client {
            name: name.to_string(),
            transport: Transport::Socket { tx, pending },
            next: AtomicU64::new(1),
            timeout,
            protocol: PROTOCOL.to_string(),
        }
    }

    pub async fn initialize(&self) -> Result<Vec<Value>> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": self.protocol,
                "capabilities": {},
                "clientInfo": { "name": "opencode", "version": crate::VERSION }
            }),
        )
        .await?;
        self.notify("notifications/initialized").await?;
        self.tool_list().await
    }

    pub async fn tool_list(&self) -> Result<Vec<Value>> {
        self.paginate("tools/list", "tools").await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let resp = match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                {
                    let mut w = stdin.lock().await;
                    w.write_all(format!("{msg}\n").as_bytes()).await?;
                    w.flush().await?;
                }
                tokio::time::timeout(self.timeout, rx).await.map_err(|_| anyhow!("MCP server {} timed out on {method}", self.name))?.map_err(|_| anyhow!("MCP server {} closed", self.name))?
            }
            Transport::Http { url, headers, session } => {
                let mut rb = crate::http::client()
                    .post(url)
                    .header("Accept", "application/json, text/event-stream")
                    .header("Content-Type", "application/json")
                    .header("MCP-Protocol-Version", &self.protocol)
                    .timeout(self.timeout)
                    .json(&msg);
                for (k, v) in headers {
                    rb = rb.header(k, v);
                }
                if let Some(s) = session.lock().unwrap().clone() {
                    rb = rb.header("Mcp-Session-Id", s);
                }
                let r = crate::http::send(rb).await?;
                if let Some(s) = r.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                    *session.lock().unwrap() = Some(s.to_string());
                }
                if !r.status().is_success() {
                    return Err(anyhow!("MCP server {} returned HTTP {}", self.name, r.status().as_u16()));
                }
                let is_sse = r.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("event-stream"));
                if is_sse {
                    let mut sse = crate::provider::Sse::new(r);
                    let mut found = None;
                    while let Some((_, data)) = sse.next().await? {
                        if let Ok(v) = serde_json::from_str::<Value>(&data) {
                            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                                found = Some(v);
                                break;
                            }
                        }
                    }
                    found.ok_or_else(|| anyhow!("MCP server {} sent no response", self.name))?
                } else {
                    r.json::<Value>().await?
                }
            }
            Transport::Socket { tx, pending } => {
                let (reply, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, reply);
                tx.send(msg.to_string()).map_err(|_| anyhow!("MCP client {} disconnected", self.name))?;
                tokio::time::timeout(self.timeout, rx).await.map_err(|_| anyhow!("MCP client {} timed out on {method}", self.name))?.map_err(|_| anyhow!("MCP client {} disconnected", self.name))?
            }
        };
        if let Some(e) = resp.get("error") {
            return Err(anyhow!("MCP error from {}: {}", self.name, e.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn paginate(&self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request(method, params).await?;
            out.extend(result.get(key).and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = result.get("nextCursor").and_then(Value::as_str).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn resources(&self) -> Result<(Vec<Value>, Vec<Value>)> {
        let resources = self.paginate("resources/list", "resources").await?;
        let templates = self.paginate("resources/templates/list", "resourceTemplates").await?;
        Ok((resources, templates))
    }

    async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.request("resources/read", json!({ "uri": uri })).await
    }

    async fn prompts(&self) -> Result<Vec<Value>> {
        self.paginate("prompts/list", "prompts").await
    }

    async fn prompt(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request("prompts/get", json!({ "name": name, "arguments": arguments })).await
    }

    async fn notify(&self, method: &str) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut w = stdin.lock().await;
                w.write_all(format!("{msg}\n").as_bytes()).await?;
                w.flush().await?;
            }
            Transport::Http { url, headers, session } => {
                let mut rb = crate::http::client().post(url).header("Accept", "application/json, text/event-stream").json(&msg);
                for (k, v) in headers {
                    rb = rb.header(k, v);
                }
                if let Some(s) = session.lock().unwrap().clone() {
                    rb = rb.header("Mcp-Session-Id", s);
                }
                let _ = crate::http::send(rb).await;
            }
            Transport::Socket { tx, .. } => {
                let _ = tx.send(msg.to_string());
            }
        }
        Ok(())
    }
}

/// On Windows, `.cmd`/`.bat` launchers (npx, uvx shims) must run through `cmd /C`.
fn spawn_command(prog: &str) -> tokio::process::Command {
    #[cfg(windows)]
    if crate::util::which(prog).is_some_and(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(prog);
        return c;
    }
    tokio::process::Command::new(prog)
}

async fn connect(name: &str, cfg: &McpConfig, dir: &PathBuf) -> Result<(Arc<Client>, Vec<Value>)> {
    let timeout = std::time::Duration::from_millis(
        cfg.timeout
            .as_ref()
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.get("startup").and_then(serde_json::Value::as_u64))
            })
            .unwrap_or(30_000),
    );
    let kind = cfg.kind.clone().unwrap_or_else(|| if cfg.url.is_some() { "remote".into() } else { "local".into() });
    let transport = if kind == "remote" {
        let url = cfg.url.clone().ok_or_else(|| anyhow!("remote MCP server {name} has no url"))?;
        let mut headers = cfg.headers.clone();
        if let Ok(Some(token)) = crate::mcp_oauth::access_token(name, cfg).await {
            headers.insert("authorization".into(), format!("Bearer {token}"));
        }
        Transport::Http { url, headers, session: Mutex::new(None) }
    } else {
        let (prog, args) = cfg.command.split_first().ok_or_else(|| anyhow!("local MCP server {name} has no command"))?;
        let mut cmd = spawn_command(prog);
        let cwd = cfg
            .cwd
            .as_deref()
            .map(|cwd| crate::util::resolve(dir, cwd))
            .unwrap_or_else(|| dir.clone());
        cmd.args(args).envs(&cfg.environment).current_dir(cwd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| anyhow!("failed to start MCP server {name}: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    if v.get("method").is_none() {
                        if let Some(tx) = p2.lock().unwrap().remove(&id) {
                            let _ = tx.send(v);
                        }
                    }
                }
            }
        });
        Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), pending, _child: child }
    };
    let protocol = cfg.protocol.clone().unwrap_or_else(|| PROTOCOL.to_string());
    let client = Arc::new(Client { name: name.to_string(), transport, next: AtomicU64::new(1), timeout, protocol: protocol.clone() });
    client
        .request("initialize", json!({ "protocolVersion": protocol, "capabilities": {}, "clientInfo": { "name": "opencode", "version": crate::VERSION } }))
        .await?;
    client.notify("notifications/initialized").await?;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let r = client.request("tools/list", params).await?;
        tools.extend(r.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default());
        cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    Ok((client, tools))
}

pub struct McpTool {
    client: Arc<Client>,
    full_name: String,
    tool: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        ctx.ask(&self.full_name, &["*".into()], &["*".into()], format!("Call MCP tool {}", self.full_name), input.clone()).await?;
        let r = tokio::select! {
            r = self.client.request("tools/call", json!({ "name": self.tool, "arguments": input })) => r?,
            _ = ctx.cancel.cancelled() => return Err(anyhow!("cancelled")),
        };
        let mut out = Output::text("", self.full_name.clone());
        let mut texts = Vec::new();
        for c in r.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("text") => texts.push(c.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Some("image") => out.attachments.push(crate::session::FilePart {
                    mime: c.get("mimeType").and_then(|m| m.as_str()).unwrap_or("image/png").into(),
                    data: c.get("data").and_then(|d| d.as_str()).unwrap_or("").into(),
                    name: self.full_name.clone(),
                }),
                Some("resource") => texts.push(c.pointer("/resource/text").and_then(|t| t.as_str()).map(String::from).unwrap_or_else(|| c.to_string())),
                _ => texts.push(c.to_string()),
            }
        }
        if texts.is_empty() {
            if let Some(sc) = r.get("structuredContent") {
                texts.push(sc.to_string());
            }
        }
        let text = texts.join("\n");
        if r.get("isError").and_then(|e| e.as_bool()) == Some(true) {
            return Err(anyhow!(if text.is_empty() { "MCP tool returned an error".to_string() } else { text }));
        }
        out.output = text;
        Ok(out)
    }
}

pub struct Manager {
    configs: std::sync::RwLock<BTreeMap<String, McpConfig>>,
    dir: PathBuf,
    tools: std::sync::Mutex<Option<Vec<Arc<dyn Tool>>>>,
    clients: Mutex<BTreeMap<String, Arc<Client>>>,
    dynamic: Mutex<BTreeMap<String, (Arc<Client>, Vec<Value>)>>,
    status: Mutex<BTreeMap<String, Status>>,
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

impl Manager {
    pub fn new(configs: BTreeMap<String, McpConfig>, dir: PathBuf) -> Self {
        Manager {
            configs: std::sync::RwLock::new(configs),
            dir,
            tools: std::sync::Mutex::new(None),
            clients: Mutex::new(BTreeMap::new()),
            dynamic: Mutex::new(BTreeMap::new()),
            status: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replace the server set, reconnecting lazily on the next tool lookup.
    pub fn reconfigure(&self, configs: BTreeMap<String, McpConfig>) {
        if *self.configs.read().unwrap() == configs {
            return;
        }
        *self.configs.write().unwrap() = configs;
        *self.tools.lock().unwrap() = None;
        self.clients.lock().unwrap().clear();
        self.status.lock().unwrap().clear();
    }

    /// Add or replace one server definition without touching the others.
    pub fn set_server(&self, name: &str, config: McpConfig) {
        let mut configs = self.configs.read().unwrap().clone();
        configs.insert(name.to_string(), config);
        self.clients.lock().unwrap().remove(name);
        self.status.lock().unwrap().remove(name);
        *self.tools.lock().unwrap() = None;
        *self.configs.write().unwrap() = configs;
    }

    pub fn remove_server(&self, name: &str) {
        let mut configs = self.configs.read().unwrap().clone();
        configs.remove(name);
        self.clients.lock().unwrap().remove(name);
        self.status.lock().unwrap().remove(name);
        *self.tools.lock().unwrap() = None;
        *self.configs.write().unwrap() = configs;
    }

    pub async fn connect_server(&self, name: &str) -> Result<Status> {
        *self.tools.lock().unwrap() = None;
        self.clients.lock().unwrap().remove(name);
        self.status.lock().unwrap().remove(name);
        self.tools().await;
        Ok(self
            .status
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or(Status::Disabled))
    }

    pub fn disconnect_server(&self, name: &str) {
        self.clients.lock().unwrap().remove(name);
        self.status.lock().unwrap().insert(name.to_string(), Status::Disabled);
        *self.tools.lock().unwrap() = None;
    }

    pub async fn attach_socket(&self, name: &str, client: Arc<Client>) -> Result<Vec<Value>> {
        let listed = client.initialize().await?;
        self.dynamic.lock().unwrap().insert(name.to_string(), (client, listed.clone()));
        *self.tools.lock().unwrap() = None;
        self.tools().await;
        Ok(listed)
    }

    pub fn detach_socket(&self, name: &str) {
        self.dynamic.lock().unwrap().remove(name);
        self.status.lock().unwrap().insert(name.to_string(), Status::Disabled);
        *self.tools.lock().unwrap() = None;
    }

    pub async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if let Some(tools) = self.tools.lock().unwrap().clone() {
            return tools;
        }
        let configs = self.configs.read().unwrap().clone();
        let mut all: Vec<Arc<dyn Tool>> = Vec::new();
        let futs = configs.iter().map(|(name, cfg)| async move {
            if cfg.enabled == Some(false) {
                return (name.clone(), Err(None));
            }
            (name.clone(), connect(name, cfg, &self.dir).await.map_err(Some))
        });
        for (name, res) in futures::future::join_all(futs).await {
            match res {
                Ok((client, tools)) => {
                    self.clients.lock().unwrap().insert(name.clone(), client.clone());
                    self.status.lock().unwrap().insert(name.clone(), Status::Connected { tools: tools.len() });
                    for t in tools {
                        let tool = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        all.push(Arc::new(McpTool {
                            client: client.clone(),
                            full_name: format!("{}_{}", sanitize(&name), sanitize(&tool)),
                            description: t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                            schema: t.get("inputSchema").cloned().unwrap_or(json!({ "type": "object", "properties": {} })),
                            tool,
                        }));
                    }
                }
                Err(None) => {
                    self.clients.lock().unwrap().remove(&name);
                    self.status.lock().unwrap().insert(name, Status::Disabled);
                }
                Err(Some(e)) => {
                    self.clients.lock().unwrap().remove(&name);
                    self.status.lock().unwrap().insert(name, Status::Failed(e.to_string()));
                }
            }
        }
        let dynamic = self.dynamic.lock().unwrap().clone();
        for (name, (client, listed)) in dynamic {
            self.status.lock().unwrap().insert(name.clone(), Status::Connected { tools: listed.len() });
            for t in listed {
                let tool = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                all.push(Arc::new(McpTool {
                    client: client.clone(),
                    full_name: format!("{}_{}", sanitize(&name), sanitize(&tool)),
                    description: t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                    schema: t.get("inputSchema").cloned().unwrap_or(json!({ "type": "object", "properties": {} })),
                    tool,
                }));
            }
        }
        *self.tools.lock().unwrap() = Some(all.clone());
        all
    }

    pub async fn status(&self) -> BTreeMap<String, Status> {
        self.tools().await;
        self.status.lock().unwrap().clone()
    }

    pub fn configured(&self) -> usize {
        self.configs.read().unwrap().len()
    }

    pub fn server_names(&self) -> Vec<String> {
        self.configs.read().unwrap().keys().cloned().collect()
    }

    async fn connected(&self) -> BTreeMap<String, Arc<Client>> {
        self.tools().await;
        self.clients.lock().unwrap().clone()
    }

    pub async fn resource_catalog(&self, server: Option<&str>) -> Result<Value> {
        let clients = self.connected().await;
        let mut resources = Vec::new();
        let mut templates = Vec::new();
        for (name, client) in clients {
            if server.is_some_and(|server| server != name.as_str()) {
                continue;
            }
            let (mut listed, mut templated) = client.resources().await?;
            for item in listed.iter_mut().chain(templated.iter_mut()) {
                item["server"] = json!(name);
            }
            resources.extend(listed);
            templates.extend(templated);
        }
        Ok(json!({ "resources": resources, "templates": templates }))
    }

    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Option<Value>> {
        let clients = self.connected().await;
        let Some(client) = clients.get(server) else { return Ok(None) };
        Ok(Some(client.read_resource(uri).await?))
    }

    pub async fn prompts(&self) -> Result<Vec<Value>> {
        let clients = self.connected().await;
        let mut prompts = Vec::new();
        for (name, client) in clients {
            let mut listed = client.prompts().await?;
            for item in listed.iter_mut() {
                item["server"] = json!(name);
            }
            prompts.extend(listed);
        }
        Ok(prompts)
    }

    pub async fn prompt(&self, server: &str, name: &str, arguments: Value) -> Result<Option<Value>> {
        let clients = self.connected().await;
        let Some(client) = clients.get(server) else { return Ok(None) };
        Ok(Some(client.prompt(name, arguments).await?))
    }

    /// Configured servers with their status so far (`None` while still connecting).
    pub fn snapshot(&self) -> Vec<(String, Option<Status>)> {
        let st = self.status.lock().unwrap();
        self.configs.read().unwrap().keys().map(|n| (n.clone(), st.get(n).cloned())).collect()
    }
}
