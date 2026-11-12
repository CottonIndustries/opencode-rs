//! `opencode acp`: Agent Client Protocol server over stdio (JSON-RPC 2.0, one
//! message per line), ported from the TypeScript client's ACP agent.

use crate::app::{App, AppEvent, PromptOptions};
use crate::config::McpConfig;
use crate::permission::Reply;
use crate::session::{Message, Part, Role, ToolState};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const LOGIN_METHOD: &str = "opencode-login";
const DEFAULT_EFFORT: &str = "default";

/// A JSON-RPC error returned to the client.
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        RpcError { code: -32602, message: message.into() }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        RpcError { code: -32603, message: e.to_string() }
    }
}

type RpcResult = std::result::Result<Value, RpcError>;

/// An ACP session bound to the app for its working directory.
struct Attached {
    app: Arc<App>,
    cwd: PathBuf,
    variant: Mutex<Option<String>>,
    prompting: AtomicBool,
    cancelled: AtomicBool,
}

struct Server {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    apps: tokio::sync::Mutex<HashMap<String, Arc<App>>>,
    sessions: Mutex<HashMap<String, Arc<Attached>>>,
}

pub async fn serve() -> Result<()> {
    // the TypeScript client reports itself as `acp` in this mode
    std::env::set_var("OPENCODE_CLIENT", "acp");
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let server = Arc::new(Server {
        out: tx,
        next_id: AtomicU64::new(0),
        pending: Mutex::new(HashMap::new()),
        apps: tokio::sync::Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                server.send(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } }));
                continue;
            }
        };
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(|m| m.as_str()).map(String::from) {
            Some(method) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let s = server.clone();
                tokio::spawn(async move {
                    match id {
                        Some(id) => s.request(id, &method, params).await,
                        None => s.notification(&method, params),
                    }
                });
            }
            None => {
                // a response to one of our requests
                if let Some(n) = id.as_ref().and_then(|i| i.as_u64()) {
                    if let Some(tx) = server.pending.lock().unwrap().remove(&n) {
                        let _ = tx.send(msg);
                    }
                }
            }
        }
    }
    let apps: Vec<Arc<crate::app::App>> = server.apps.lock().await.values().cloned().collect();
    for app in apps {
        app.plugins.flush().await;
        app.plugins.shutdown().await;
    }
    drop(server);
    let _ = writer.await;
    Ok(())
}

impl Server {
    fn send(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }

    fn update(&self, session_id: &str, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session_id, "update": update } }));
    }

    /// Send a request to the client and wait for its result.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let resp = rx.await.map_err(|_| anyhow!("connection closed"))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("{}", err.get("message").and_then(|m| m.as_str()).unwrap_or("client error")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn request(self: &Arc<Self>, id: Value, method: &str, params: Value) {
        let mut after = Vec::new();
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "authenticate" => match params.get("methodId").and_then(|m| m.as_str()) {
                Some(LOGIN_METHOD) => Ok(json!({})),
                m => Err(RpcError::invalid_params(format!("unknown auth method: {}", m.unwrap_or("")))),
            },
            "session/new" => self.new_session(&params, &mut after).await,
            "session/load" => self.load_session(&params, true, &mut after).await,
            "session/resume" => self.load_session(&params, false, &mut after).await,
            "session/fork" => self.fork_session(&params, &mut after).await,
            "session/list" => self.list_sessions(&params),
            "session/delete" => self.delete_session(&params),
            "session/close" => self.close_session(&params),
            "session/set_config_option" => self.set_config_option(&params),
            "session/set_mode" => self.set_mode(&params),
            "session/prompt" => self.prompt(&params).await,
            _ => Err(RpcError { code: -32601, message: format!("Method not found: {method}") }),
        };
        match result {
            Ok(r) => self.send(json!({ "jsonrpc": "2.0", "id": id, "result": r })),
            Err(e) => self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": e.code, "message": e.message } })),
        }
        for (sid, update) in after {
            self.update(&sid, update);
        }
    }

    fn notification(&self, method: &str, params: Value) {
        if method != "session/cancel" {
            return;
        }
        if let Ok(sid) = str_param(&params, "sessionId") {
            if let Ok(a) = self.attached(sid) {
                cancel(&a, sid);
            }
        }
    }

    fn attached(&self, session_id: &str) -> std::result::Result<Arc<Attached>, RpcError> {
        self.sessions.lock().unwrap().get(session_id).cloned().ok_or_else(|| RpcError::invalid_params(format!("Session not found: {session_id}")))
    }

    /// One app per working directory and set of client MCP servers.
    async fn app_for(&self, params: &Value) -> std::result::Result<(Arc<App>, PathBuf), RpcError> {
        let cwd = str_param(params, "cwd")?;
        if !Path::new(cwd).is_absolute() {
            return Err(RpcError::invalid_params(format!("cwd must be an absolute path: {cwd}")));
        }
        let mcp = mcp_servers(params.get("mcpServers"))?;
        let key = format!("{cwd}\n{}", serde_json::to_string(&params.get("mcpServers")).unwrap_or_default());
        let mut apps = self.apps.lock().await;
        let app = match apps.get(&key) {
            Some(a) => a.clone(),
            None => {
                let a = App::init_with_mcp(PathBuf::from(cwd), mcp).await?;
                apps.insert(key, a.clone());
                a
            }
        };
        let dir = app.directory.clone();
        Ok((app, dir))
    }

    fn attach(&self, session_id: &str, app: Arc<App>, cwd: PathBuf) -> Arc<Attached> {
        let a = Arc::new(Attached { app, cwd, variant: Mutex::new(None), prompting: AtomicBool::new(false), cancelled: AtomicBool::new(false) });
        self.sessions.lock().unwrap().insert(session_id.to_string(), a.clone());
        a
    }

    async fn new_session(&self, params: &Value, after: &mut Vec<(String, Value)>) -> RpcResult {
        let (app, cwd) = self.app_for(params).await?;
        let handle = app.new_session(None);
        let sid = handle.id();
        let a = self.attach(&sid, app, cwd);
        after.push((sid.clone(), commands_update(&a.app)));
        Ok(json!({ "sessionId": sid, "configOptions": config_options(&a, &sid) }))
    }

    /// `session/load` (replays history) and `session/resume`.
    async fn load_session(&self, params: &Value, replay: bool, after: &mut Vec<(String, Value)>) -> RpcResult {
        let sid = str_param(params, "sessionId")?.to_string();
        let (app, cwd) = self.app_for(params).await?;
        let handle = app.session(&sid).map_err(|_| RpcError { code: -32002, message: format!("Resource not found: {sid}") })?;
        if crate::util::normalize(&handle.with(|s| s.directory.clone())) != crate::util::normalize(&cwd) {
            return Err(RpcError::invalid_params(format!("Session {sid} does not belong to {}", cwd.display())));
        }
        let a = self.attach(&sid, app, cwd);
        if replay {
            let messages = handle.with(|s| s.messages.clone());
            for m in &messages {
                for u in history_updates(m, &a.cwd) {
                    self.update(&sid, u);
                }
            }
        }
        after.push((sid.clone(), commands_update(&a.app)));
        Ok(json!({ "configOptions": config_options(&a, &sid) }))
    }

    async fn fork_session(&self, params: &Value, after: &mut Vec<(String, Value)>) -> RpcResult {
        let sid = str_param(params, "sessionId")?.to_string();
        let (app, cwd) = self.app_for(params).await?;
        let source = app.session(&sid).map_err(|_| RpcError { code: -32002, message: format!("Resource not found: {sid}") })?;
        let forked = crate::session::fork(&source.session.lock().unwrap());
        crate::session::save(&forked)?;
        let a = self.attach(&forked.id, app, cwd);
        after.push((forked.id.clone(), commands_update(&a.app)));
        Ok(json!({ "sessionId": forked.id, "configOptions": config_options(&a, &forked.id) }))
    }

    fn list_sessions(&self, params: &Value) -> RpcResult {
        let cwd = params.get("cwd").and_then(|c| c.as_str()).map(|c| crate::util::normalize(Path::new(c)));
        let root = crate::paths::get().storage().join("session");
        let mut all = Vec::new();
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let project = e.file_name().to_string_lossy().to_string();
            all.extend(
                crate::session::list(&project)
                    .into_iter()
                    .filter(|s| s.parent_id.is_none())
                    .filter(|s| cwd.as_ref().map_or(true, |c| crate::util::normalize(&s.directory) == *c)),
            );
        }
        all.sort_by(|a, b| b.time_updated.cmp(&a.time_updated));
        let sessions: Vec<Value> = all
            .into_iter()
            .take(100)
            .map(|s| {
                json!({
                    "sessionId": s.id,
                    "cwd": s.directory,
                    "title": s.title,
                    "updatedAt": chrono::DateTime::from_timestamp_millis(s.time_updated).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                })
            })
            .collect();
        Ok(json!({ "sessions": sessions }))
    }

    fn delete_session(&self, params: &Value) -> RpcResult {
        let sid = str_param(params, "sessionId")?;
        if let Some(a) = self.sessions.lock().unwrap().remove(sid) {
            cancel(&a, sid);
            a.app.forget_session(sid);
        }
        let root = crate::paths::get().storage().join("session");
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let project = e.file_name().to_string_lossy().to_string();
            if e.path().join(format!("{sid}.json")).is_file() {
                crate::session::delete(&project, sid)?;
            }
        }
        Ok(json!({}))
    }

    fn close_session(&self, params: &Value) -> RpcResult {
        let sid = str_param(params, "sessionId")?;
        if let Some(a) = self.sessions.lock().unwrap().remove(sid) {
            cancel(&a, sid);
        }
        Ok(json!({}))
    }

    fn set_config_option(&self, params: &Value) -> RpcResult {
        let sid = str_param(params, "sessionId")?;
        let a = self.attached(sid)?;
        let config_id = str_param(params, "configId")?;
        let value = params.get("value").and_then(|v| v.as_str()).ok_or_else(|| RpcError::invalid_params(format!("Invalid value for {config_id}")))?;
        select(&a, sid, config_id, value)?;
        Ok(json!({ "configOptions": config_options(&a, sid) }))
    }

    fn set_mode(&self, params: &Value) -> RpcResult {
        let sid = str_param(params, "sessionId")?;
        let a = self.attached(sid)?;
        select(&a, sid, "mode", str_param(params, "modeId")?)?;
        Ok(json!({}))
    }

    async fn prompt(self: &Arc<Self>, params: &Value) -> RpcResult {
        let sid = str_param(params, "sessionId")?.to_string();
        let a = self.attached(&sid)?;
        if a.prompting.swap(true, Ordering::SeqCst) {
            return Err(RpcError { code: -32603, message: format!("Session already has an active ACP prompt: {sid}") });
        }
        a.cancelled.store(false, Ordering::SeqCst);
        let result = self.run_turn(&a, &sid, params.get("prompt").cloned().unwrap_or(Value::Null)).await;
        a.prompting.store(false, Ordering::SeqCst);
        result
    }

    async fn run_turn(self: &Arc<Self>, a: &Arc<Attached>, sid: &str, prompt: Value) -> RpcResult {
        let app = a.app.clone();
        let blocks = prompt.as_array().cloned().unwrap_or_default();
        let input = convert_prompt(&blocks, &a.cwd)?;
        let mut opts = PromptOptions { agent: None, model: None, variant: a.variant.lock().unwrap().clone() };
        let mut text = input.text.clone();

        // slash commands: built-in /compact, then custom commands
        if let Some((name, args)) = slash(&text) {
            if name == "compact" {
                let mut rx = app.subscribe();
                let compact = crate::engine::compact_session(app.clone(), sid);
                tokio::pin!(compact);
                let res = loop {
                    tokio::select! {
                        r = &mut compact => break r,
                        ev = rx.recv() => if let Ok(AppEvent::Error { session, message }) = ev {
                            if session == sid { self.update(sid, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": message } })); }
                        },
                    }
                };
                return match res {
                    Ok(()) => Ok(json!({ "stopReason": "end_turn" })),
                    Err(e) => Err(e.into()),
                };
            }
            if app.plugin_commands().contains(&name) {
                app.plugins.execute_command(&name, sid, &args).await?;
                return Ok(json!({ "stopReason": "end_turn" }));
            }
            let commands = app.commands();
            if let Some(cmd) = commands.get(&name) {
                text = crate::command::render(cmd, &args, &app.directory, &app.worktree);
                opts.agent = cmd.agent.clone();
                opts.model = cmd.model.clone();
            }
        }

        let handle = app.session(sid)?;
        let mut msg = Message::user_text(text);
        msg.parts.extend(input.files);
        for s in input.synthetic {
            msg.parts.push(Part::Text { text: s, synthetic: true });
        }
        let start = handle.with(|s| s.messages.len());

        let mut rx = app.subscribe();
        let (app2, sid2) = (app.clone(), sid.to_string());
        let task = tokio::spawn(async move { crate::engine::prompt(app2, &sid2, msg, opts).await });
        tokio::pin!(task);
        let mut tools: HashMap<String, (String, Value)> = HashMap::new();
        let mut open: HashSet<String> = HashSet::new();
        let mut error: Option<String> = None;
        let mut done = false;
        let mut prompt_result: Option<Result<()>> = None;
        loop {
            tokio::select! {
                r = &mut task, if !done => {
                    done = true;
                    prompt_result = Some(r.unwrap_or_else(|e| Err(anyhow!("{e}"))));
                    // a prompt queued behind a running turn returns at once; wait for the session to go idle
                    if !handle.is_busy() { break; }
                }
                ev = rx.recv() => {
                    let ev = match ev {
                        Ok(ev) => ev,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    };
                    if matches!(&ev, AppEvent::Busy { session, busy: false } if session == sid) && done {
                        break;
                    }
                    self.forward(a, sid, ev, &mut tools, &mut open, &mut error);
                }
            }
        }
        // events emitted just before the turn ended
        while let Ok(ev) = rx.try_recv() {
            self.forward(a, sid, ev, &mut tools, &mut open, &mut error);
        }

        let cancelled = a.cancelled.load(Ordering::SeqCst);
        if cancelled {
            for id in open.drain() {
                let (name, input) = tools.get(&id).cloned().unwrap_or_else(|| ("tool".into(), json!({})));
                self.update(sid, failed_update(&id, &name, &input, "Cancelled", &Value::Null, &a.cwd));
            }
        }
        if !cancelled {
            if let Some(Err(e)) = prompt_result {
                return Err(e.into());
            }
            if let Some(e) = error {
                return Err(RpcError { code: -32603, message: e });
            }
        }

        let (turn, last_finish, cost, model) = handle.with(|s| {
            let new: Vec<&Message> = s.messages.iter().skip(start).filter(|m| m.role == Role::Assistant).collect();
            let mut turn = crate::session::Usage::default();
            for m in &new {
                turn.input += m.usage.input;
                turn.output += m.usage.output;
                turn.reasoning += m.usage.reasoning;
                turn.cache_read += m.usage.cache_read;
                turn.cache_write += m.usage.cache_write;
            }
            let last = new.last().map(|m| (m.usage.clone(), m.finish.clone()));
            (turn, last, s.messages.iter().map(|m| m.cost).sum::<f64>(), s.model.clone())
        });
        if let Some((last, _)) = &last_finish {
            let context = model
                .and_then(|m| crate::models::resolve(&app.providers(), &m).ok())
                .map(|r| r.model.context)
                .unwrap_or(0);
            let used = total(last);
            if used > 0 && context > 0 {
                self.update(sid, json!({ "sessionUpdate": "usage_update", "used": used, "size": context, "cost": { "amount": cost, "currency": "USD" } }));
            }
        }
        let finish = last_finish.and_then(|(_, f)| f);
        let stop = if cancelled {
            "cancelled"
        } else {
            match finish.as_deref() {
                Some("length") => "max_tokens",
                Some("refusal") | Some("content-filter") => "refusal",
                _ => "end_turn",
            }
        };
        let mut resp = json!({ "stopReason": stop });
        if total(&turn) > 0 {
            let mut u = json!({ "inputTokens": turn.input, "outputTokens": turn.output, "totalTokens": total(&turn) });
            if turn.reasoning > 0 {
                u["thoughtTokens"] = json!(turn.reasoning);
            }
            if turn.cache_read > 0 {
                u["cachedReadTokens"] = json!(turn.cache_read);
            }
            if turn.cache_write > 0 {
                u["cachedWriteTokens"] = json!(turn.cache_write);
            }
            resp["usage"] = u;
        }
        Ok(resp)
    }

    /// Translate one app event into ACP updates for `sid`.
    fn forward(
        self: &Arc<Self>,
        a: &Arc<Attached>,
        sid: &str,
        ev: AppEvent,
        tools: &mut HashMap<String, (String, Value)>,
        open: &mut HashSet<String>,
        error: &mut Option<String>,
    ) {
        if let AppEvent::Permission(req) = &ev {
            if req.session_id == sid || is_descendant(&a.app, &req.session_id, sid) {
                let tool = req.tool_call_id.as_ref().and_then(|id| tools.get(id)).cloned();
                let (s, a, req) = (self.clone(), a.clone(), req.clone());
                let sid = sid.to_string();
                tokio::spawn(async move { s.ask_permission(&a, &sid, req, tool).await });
            }
            return;
        }
        if ev.session() != Some(sid) {
            return;
        }
        match ev {
            AppEvent::TextDelta { delta, .. } => {
                self.update(sid, json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": delta } }))
            }
            AppEvent::ReasoningDelta { delta, .. } => {
                self.update(sid, json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": delta } }))
            }
            AppEvent::ToolStarted { id, name, input, .. } => {
                tools.insert(id.clone(), (name.clone(), input.clone()));
                open.insert(id.clone());
                let mut call = tool_call(&id, &name, &input, &a.cwd);
                call["status"] = json!("in_progress");
                call["sessionUpdate"] = json!("tool_call");
                self.update(sid, call);
            }
            AppEvent::ToolFinished { id, name, output, error: err, metadata, .. } => {
                open.remove(&id);
                let input = tools.get(&id).map(|(_, i)| i.clone()).unwrap_or_else(|| json!({}));
                let update = match err {
                    Some(e) => failed_update(&id, &name, &input, &e, &metadata, &a.cwd),
                    None => completed_update(&id, &name, &input, &output, &metadata, &a.cwd),
                };
                self.update(sid, update);
            }
            AppEvent::Error { message, .. } => *error = Some(message),
            _ => {}
        }
    }

    async fn ask_permission(&self, a: &Attached, sid: &str, req: crate::permission::Request, tool: Option<(String, Value)>) {
        // described by the asking tool when known, else by the permission itself
        let (name, input) = tool.clone().unwrap_or_else(|| (req.action.clone(), req.metadata.clone()));
        let tool = tool.is_some();
        let diffs = permission_diffs(&req.metadata, &a.cwd);
        let title = match diffs.len() {
            n if n > 1 => format!("{n} files"),
            _ => permission_title(&name, &input).or_else(|| diffs.first().and_then(|d| d["path"].as_str().map(String::from))).unwrap_or_else(|| req.title.clone()),
        };
        let mut call = tool_call(req.tool_call_id.as_deref().unwrap_or(&req.id), &name, &input, &a.cwd);
        call["title"] = json!(title);
        call["status"] = json!("pending");
        if !tool {
            call.as_object_mut().map(|o| o.remove("rawInput"));
        }
        let mut locations = call["locations"].as_array().cloned().unwrap_or_default();
        if locations.is_empty() && matches!(req.action.as_str(), "read" | "edit" | "external_directory") {
            for r in &req.resources {
                let r = r.strip_suffix("/*").unwrap_or(r);
                if !r.is_empty() && !r.contains(['*', '?']) {
                    locations.push(json!({ "path": abs(r, &a.cwd) }));
                }
            }
            call["locations"] = Value::Array(locations);
        }
        if !diffs.is_empty() {
            call["content"] = Value::Array(diffs);
        }
        let options = json!([
            { "optionId": "once", "kind": "allow_once", "name": "Allow once" },
            { "optionId": "always", "kind": "allow_always", "name": "Always allow" },
            { "optionId": "reject", "kind": "reject_once", "name": "Reject" },
        ]);
        let res = self.call("session/request_permission", json!({ "sessionId": sid, "toolCall": call, "options": options })).await;
        let choice = res.ok().and_then(|r| {
            (r.pointer("/outcome/outcome").and_then(|o| o.as_str()) == Some("selected")).then(|| r.pointer("/outcome/optionId").and_then(|o| o.as_str()).map(String::from)).flatten()
        });
        let reply = match choice.as_deref() {
            Some("once") => Reply::Once,
            Some("always") => Reply::Always,
            _ => Reply::Reject,
        };
        a.app.reply_permission(&req.id, reply);
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> std::result::Result<&'a str, RpcError> {
    params.get(key).and_then(|v| v.as_str()).ok_or_else(|| RpcError::invalid_params(format!("missing {key}")))
}

fn cancel(a: &Attached, sid: &str) {
    a.cancelled.store(true, Ordering::SeqCst);
    if let Ok(h) = a.app.session(sid) {
        h.abort();
    }
    a.app.reject_all_pending(sid);
}

fn is_descendant(app: &App, id: &str, root: &str) -> bool {
    let mut cur = id.to_string();
    for _ in 0..16 {
        let parent = app.session(&cur).ok().and_then(|h| h.with(|s| s.parent_id.clone()));
        match parent {
            Some(p) if p == root => return true,
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

fn initialize(params: &Value) -> Value {
    let caps = params.get("clientCapabilities").cloned().unwrap_or(Value::Null);
    let mut method = json!({ "description": "Run `opencode auth login` in the terminal", "name": "Login with opencode", "id": LOGIN_METHOD });
    if caps.pointer("/auth/terminal").and_then(|v| v.as_bool()) == Some(true) {
        method["type"] = json!("terminal");
        method["args"] = json!(["--login"]);
    }
    if caps.pointer("/_meta/terminal-auth").and_then(|v| v.as_bool()) == Some(true) {
        method["_meta"] = json!({ "terminal-auth": { "command": "opencode", "args": ["auth", "login"], "label": "OpenCode Login" } });
    }
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "mcpCapabilities": { "http": true, "sse": false },
            "promptCapabilities": { "embeddedContext": true, "image": true },
            "sessionCapabilities": { "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {} },
        },
        "authMethods": [method],
        "agentInfo": { "name": "OpenCode", "version": crate::UPSTREAM_VERSION },
    })
}

/// Client-supplied MCP servers (stdio and HTTP) as config entries.
fn mcp_servers(v: Option<&Value>) -> std::result::Result<BTreeMap<String, McpConfig>, RpcError> {
    let mut out = BTreeMap::new();
    let pairs = |v: Option<&Value>| -> BTreeMap<String, String> {
        v.and_then(|x| x.as_array())
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p.get("name")?.as_str()?.to_string(), p.get("value")?.as_str()?.to_string())))
            .collect()
    };
    for s in v.and_then(|x| x.as_array()).into_iter().flatten() {
        let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("mcp").to_string();
        let cfg = match s.get("type").and_then(|t| t.as_str()) {
            None | Some("stdio") => {
                let mut command = vec![s.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string()];
                command.extend(s.get("args").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|a| a.as_str().map(String::from)));
                McpConfig { kind: Some("local".into()), command, environment: pairs(s.get("env")), url: None, headers: BTreeMap::new(), enabled: None, timeout: None }
            }
            Some("http") => McpConfig {
                kind: Some("remote".into()),
                command: vec![],
                environment: BTreeMap::new(),
                url: s.get("url").and_then(|u| u.as_str()).map(String::from),
                headers: pairs(s.get("headers")),
                enabled: None,
                timeout: None,
            },
            _ => return Err(RpcError::invalid_params("Only stdio and HTTP MCP servers are supported")),
        };
        out.insert(name, cfg);
    }
    Ok(out)
}

fn commands_update(app: &App) -> Value {
    let mut cmds: Vec<Value> = app.commands().values().map(|c| json!({ "name": c.name, "description": c.description })).collect();
    cmds.push(json!({ "name": "compact", "description": "Compact the session" }));
    json!({ "sessionUpdate": "available_commands_update", "availableCommands": cmds })
}

fn title_case(s: &str) -> String {
    s.split(['_', '-'])
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Model, effort and mode selectors, as the TypeScript agent reports them.
fn config_options(a: &Attached, sid: &str) -> Value {
    let app = &a.app;
    let (session_model, session_agent) = app.session(sid).map(|h| h.with(|s| (s.model.clone(), s.agent.clone()))).unwrap_or((None, None));
    let providers = app.providers();
    let available = crate::models::available(&app.config, &providers);
    let mut models: Vec<&crate::models::Model> = providers.values().filter(|p| available.contains(&p.id)).flat_map(|p| p.models.values()).collect();
    models.sort_by(|x, y| x.provider_id.cmp(&y.provider_id).then_with(|| x.name.cmp(&y.name)));
    let current = session_model.or_else(|| app.default_model()).unwrap_or_default();
    let current_model = crate::models::resolve(&providers, &current).ok().map(|r| r.model);
    let mut out = vec![json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current_model.as_ref().map(|m| m.full_id()).unwrap_or(current),
        "options": models.iter().map(|m| json!({ "value": m.full_id(), "name": format!("{}/{}", m.provider_id, m.name) })).collect::<Vec<_>>(),
    })];
    let variants = current_model.as_ref().map(|m| m.variants()).unwrap_or_default();
    if !variants.is_empty() {
        let v = a.variant.lock().unwrap().clone();
        let cur = match v {
            Some(v) if variants.contains(&v) => v,
            _ => DEFAULT_EFFORT.to_string(),
        };
        let mut opts = variants.clone();
        if !opts.iter().any(|o| o == DEFAULT_EFFORT) {
            opts.push(DEFAULT_EFFORT.into());
        }
        out.push(json!({
            "id": "effort",
            "name": "Effort",
            "description": "Available effort levels for this model",
            "category": "thought_level",
            "type": "select",
            "currentValue": cur,
            "options": opts.iter().map(|o| json!({ "value": o, "name": title_case(o) })).collect::<Vec<_>>(),
        }));
    }
    out.push(json!({
        "id": "mode",
        "name": "Session Mode",
        "category": "mode",
        "type": "select",
        "currentValue": session_agent.unwrap_or_else(|| app.default_agent()),
        "options": app.primary_agents().iter().map(|ag| {
            let mut o = json!({ "value": ag.id, "name": ag.name });
            if let Some(d) = &ag.description { o["description"] = json!(d); }
            o
        }).collect::<Vec<_>>(),
    }));
    Value::Array(out)
}

fn select(a: &Attached, sid: &str, config_id: &str, value: &str) -> std::result::Result<(), RpcError> {
    let handle = a.app.session(sid)?;
    match config_id {
        "model" => {
            let r = crate::models::resolve(&a.app.providers(), value).map_err(|_| RpcError::invalid_params(format!("Unknown model: {value}")))?;
            let mut v = a.variant.lock().unwrap();
            if v.as_ref().is_some_and(|x| !r.model.variants().contains(x)) {
                *v = None;
            }
            if let Some(var) = r.variant {
                *v = Some(var);
            }
            handle.with(|s| s.model = Some(r.model.full_id()));
        }
        "effort" => {
            let model = handle.with(|s| s.model.clone()).or_else(|| a.app.default_model()).unwrap_or_default();
            let variants = crate::models::resolve(&a.app.providers(), &model).map(|r| r.model.variants()).unwrap_or_default();
            if value != DEFAULT_EFFORT && !variants.iter().any(|v| v == value) {
                return Err(RpcError::invalid_params(format!("Unknown effort: {value}")));
            }
            *a.variant.lock().unwrap() = (value != DEFAULT_EFFORT).then(|| value.to_string());
        }
        "mode" => {
            if !a.app.primary_agents().iter().any(|ag| ag.id == value) {
                return Err(RpcError::invalid_params(format!("Unknown mode: {value}")));
            }
            handle.with(|s| s.agent = Some(value.to_string()));
        }
        other => return Err(RpcError::invalid_params(format!("Unknown config option: {other}"))),
    }
    // sessions are written once they have messages
    if handle.with(|s| !s.messages.is_empty()) {
        handle.save();
    }
    Ok(())
}

struct PromptInput {
    text: String,
    files: Vec<Part>,
    synthetic: Vec<String>,
}

fn slash(text: &str) -> Option<(String, String)> {
    let t = text.trim().strip_prefix('/')?;
    let mut it = t.splitn(2, char::is_whitespace);
    let name = it.next().filter(|n| !n.is_empty())?;
    Some((name.to_string(), it.next().unwrap_or("").trim().to_string()))
}

fn file_name(uri: &str) -> String {
    let path = url::Url::parse(uri).ok().map(|u| {
        if u.scheme() == "zed" {
            u.query_pairs().find(|(k, _)| k == "path").map(|(_, v)| v.to_string()).unwrap_or_else(|| u.path().to_string())
        } else {
            u.path().to_string()
        }
    });
    let p = path.unwrap_or_else(|| uri.to_string());
    p.rsplit(['/', '\\']).next().unwrap_or("file").to_string()
}

fn url_to_path(uri: &str) -> Option<PathBuf> {
    let u = url::Url::parse(uri).ok()?;
    match u.scheme() {
        "file" => u.to_file_path().ok(),
        "zed" => u.query_pairs().find(|(k, _)| k == "path").map(|(_, v)| PathBuf::from(v.to_string())),
        _ => None,
    }
}

/// ACP content blocks → prompt text, attachments and synthetic context.
fn convert_prompt(blocks: &[Value], cwd: &Path) -> std::result::Result<PromptInput, RpcError> {
    let mut texts = Vec::new();
    let mut files = Vec::new();
    let mut synthetic = Vec::new();
    let link = |name: &str, uri: &str| if name.is_empty() { uri.to_string() } else { format!("[{name}]({uri})") };
    for b in blocks {
        let s = |k: &str| b.get(k).and_then(|v| v.as_str()).unwrap_or("");
        match s("type") {
            "text" => {
                let audience: Vec<&str> = b.pointer("/annotations/audience").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|x| x.as_str()).collect();
                match audience.as_slice() {
                    ["assistant"] => synthetic.push(s("text").to_string()),
                    ["user"] => {}
                    _ => texts.push(s("text").to_string()),
                }
            }
            "image" => {
                if !s("data").is_empty() {
                    let name = if s("uri").is_empty() { "image".to_string() } else { file_name(s("uri")) };
                    files.push(Part::File(crate::session::FilePart { mime: s("mimeType").to_string(), data: s("data").to_string(), name }));
                } else if let Some(p) = url_to_path(s("uri")) {
                    files.extend(crate::run::attach(&p, &file_name(s("uri"))).map_err(RpcError::from)?);
                } else if !s("uri").is_empty() {
                    texts.push(link("image", s("uri")));
                } else {
                    return Err(RpcError::invalid_params("image content has no data or uri"));
                }
            }
            "resource_link" => match url_to_path(s("uri")) {
                Some(p) if p.exists() => {
                    let shown = crate::util::relative(cwd, &p);
                    files.extend(crate::run::attach(&p, &shown).map_err(RpcError::from)?);
                }
                _ => texts.push(link(s("name"), s("uri"))),
            },
            "resource" => {
                let r = b.get("resource").cloned().unwrap_or(Value::Null);
                let uri = r.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                if let Some(text) = r.get("text").and_then(|t| t.as_str()) {
                    let label = match url::Url::parse(uri).ok().filter(|u| u.scheme() == "file").and_then(|u| u.to_file_path().ok().map(|p| (p, u))) {
                        Some((p, u)) => {
                            let line = u.fragment().and_then(|f| f.strip_prefix('L')).map(|f| f.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).filter(|l| !l.is_empty());
                            let p = p.to_string_lossy().replace('\\', "/");
                            match line {
                                Some(l) => format!("{p}:{l}"),
                                None => p,
                            }
                        }
                        None => uri.to_string(),
                    };
                    texts.push(format!("[{label}]\n{text}"));
                } else if let (Some(mime), Some(blob)) = (r.get("mimeType").and_then(|m| m.as_str()), r.get("blob").and_then(|m| m.as_str())) {
                    files.push(Part::File(crate::session::FilePart { mime: mime.to_string(), data: blob.to_string(), name: file_name(uri) }));
                }
            }
            _ => {}
        }
    }
    Ok(PromptInput { text: texts.join("\n"), files, synthetic })
}

fn total(u: &crate::session::Usage) -> u64 {
    u.input + u.output + u.reasoning + u.cache_read + u.cache_write
}

fn canonical_tool(name: &str) -> String {
    let n = name.to_lowercase();
    match n.as_str() {
        "bash" => "shell".into(),
        "task" => "subagent".into(),
        "apply_patch" => "patch".into(),
        _ => n,
    }
}

fn kind(name: &str) -> &'static str {
    match canonical_tool(name).as_str() {
        "shell" => "execute",
        "webfetch" => "fetch",
        "edit" | "patch" | "write" => "edit",
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => "search",
        "read" => "read",
        "subagent" => "think",
        _ => "other",
    }
}

fn abs(p: &str, cwd: &Path) -> String {
    let path = Path::new(p);
    if path.is_absolute() { p.to_string() } else { cwd.join(path).to_string_lossy().to_string() }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

fn patch_paths(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(?m)^\*\*\* (?:Add File|Update File|Delete File|Move to): (.+?)\s*$").unwrap();
    re.captures_iter(text).map(|c| c[1].to_string()).collect()
}

fn locations(name: &str, input: &Value, cwd: &Path) -> Value {
    let paths: Vec<String> = match canonical_tool(name).as_str() {
        "shell" => vec![input_str(input, "workdir").or_else(|| input_str(input, "cwd")).map(String::from).unwrap_or_else(|| cwd.to_string_lossy().to_string())],
        "read" | "edit" | "write" => input_str(input, "path").or_else(|| input_str(input, "filePath")).map(String::from).into_iter().collect(),
        "patch" => input_str(input, "patchText").map(patch_paths).unwrap_or_default(),
        "external_directory" => input_str(input, "filepath").map(String::from).into_iter().collect(),
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => input_str(input, "path").map(String::from).into_iter().collect(),
        _ => vec![],
    };
    let mut seen = HashSet::new();
    Value::Array(paths.into_iter().filter(|p| !p.is_empty()).map(|p| abs(&p, cwd)).filter(|p| seen.insert(p.clone())).map(|p| json!({ "path": p })).collect())
}

fn title(name: &str, input: &Value) -> String {
    if canonical_tool(name) == "shell" {
        if let Some(c) = input_str(input, "command").or_else(|| input_str(input, "cmd")) {
            return c.to_string();
        }
    }
    name.to_string()
}

fn raw_input(name: &str, input: &Value, cwd: &Path) -> Value {
    if canonical_tool(name) != "shell" || input.get("cwd").is_some() || input.get("workdir").is_some() {
        return input.clone();
    }
    let mut v = input.clone();
    if let Some(o) = v.as_object_mut() {
        o.insert("cwd".into(), json!(cwd));
    }
    v
}

fn tool_call(id: &str, name: &str, input: &Value, cwd: &Path) -> Value {
    json!({
        "toolCallId": id,
        "title": title(name, input),
        "kind": kind(name),
        "status": "pending",
        "locations": locations(name, input, cwd),
        "rawInput": raw_input(name, input, cwd),
    })
}

fn permission_title(name: &str, input: &Value) -> Option<String> {
    let s = |k: &str| input_str(input, k).map(String::from);
    match canonical_tool(name).as_str() {
        "external_directory" => s("description").or_else(|| s("command")).or_else(|| s("parentDir")),
        "webfetch" => s("url"),
        "websearch" => s("query"),
        "grep" | "glob" => s("pattern"),
        "read" | "edit" | "write" | "patch" => s("path").or_else(|| s("filePath")),
        _ => None,
    }
}

fn completed_update(id: &str, name: &str, input: &Value, output: &str, metadata: &Value, cwd: &Path) -> Value {
    let mut content = vec![json!({ "type": "content", "content": { "type": "text", "text": output } })];
    content.extend(result_diffs(metadata, cwd));
    let raw = if metadata.is_null() { json!({}) } else { json!({ "metadata": metadata }) };
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "completed",
        "locations": locations(name, input, cwd),
        "content": content,
        "rawOutput": raw,
    })
}

fn failed_update(id: &str, name: &str, input: &Value, error: &str, metadata: &Value, cwd: &Path) -> Value {
    let mut raw = json!({ "error": error });
    if !metadata.is_null() {
        raw["metadata"] = metadata.clone();
    }
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "failed",
        "kind": kind(name),
        "title": title(name, input),
        "locations": locations(name, input, cwd),
        "rawInput": raw_input(name, input, cwd),
        "content": [{ "type": "content", "content": { "type": "text", "text": error } }],
        "rawOutput": raw,
    })
}

fn diff_block(path: &str, old: Option<&str>, new: &str) -> Value {
    json!({ "type": "diff", "path": path, "oldText": old, "newText": new })
}

/// Diffs for a finished edit: the file now holds the new text; the old text is
/// recovered by applying the recorded hunks in reverse.
fn result_diffs(metadata: &Value, cwd: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for f in metadata.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
        let Some(file) = f.get("file").and_then(|x| x.as_str()) else { continue };
        let path = abs(file, cwd);
        let status = f.get("status").and_then(|s| s.as_str()).unwrap_or("modified");
        let current = if status == "deleted" { String::new() } else { std::fs::read_to_string(&path).unwrap_or_default() };
        if status == "added" {
            out.push(diff_block(&path, None, &current));
            continue;
        }
        let diff = f.get("diff").and_then(|d| d.as_str()).unwrap_or("");
        if let Some(old) = apply_unified(&current, diff, true) {
            out.push(diff_block(&path, Some(&old), &current));
        }
    }
    out
}

/// Diffs for a pending edit: the file still holds the old text.
fn permission_diffs(metadata: &Value, cwd: &Path) -> Vec<Value> {
    let Some(diff) = metadata.get("diff").and_then(|d| d.as_str()) else { return vec![] };
    split_diff(diff)
        .into_iter()
        .filter_map(|(file, d)| {
            let path = abs(&file, cwd);
            let old = std::fs::read_to_string(&path).ok();
            let new = apply_unified(old.as_deref().unwrap_or(""), &d, false)?;
            Some(diff_block(&path, old.as_deref(), &new))
        })
        .collect()
}

/// Split a multi-file unified diff into `(path, diff)` pairs.
fn split_diff(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = diff.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("--- ") && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ ")) {
            let path = lines[i + 1][4..].trim().to_string();
            out.push((path, String::new()));
            i += 2;
            continue;
        }
        if let Some((_, d)) = out.last_mut() {
            d.push_str(lines[i]);
            d.push('\n');
        }
        i += 1;
    }
    out
}

/// Apply unified-diff hunks to `text` (or undo them when `reverse`).
fn apply_unified(text: &str, diff: &str, reverse: bool) -> Option<String> {
    struct Hunk {
        start: usize,
        count: usize,
        from: Vec<String>,
        to: Vec<String>,
    }
    let header = regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap();
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if let Some(c) = header.captures(line) {
            let n = |i: usize, d: usize| c.get(i).map_or(d, |m| m.as_str().parse().unwrap_or(d));
            let (start, count) = if reverse { (n(3, 0), n(4, 1)) } else { (n(1, 0), n(2, 1)) };
            hunks.push(Hunk { start, count, from: vec![], to: vec![] });
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };
        let (tag, body) = match line.chars().next() {
            Some(t @ (' ' | '-' | '+')) => (t, line[1..].trim_end_matches('\r').to_string()),
            _ => continue,
        };
        let (old_side, new_side) = if reverse { ('+', '-') } else { ('-', '+') };
        if tag == ' ' || tag == old_side {
            h.from.push(body.clone());
        }
        if tag == ' ' || tag == new_side {
            h.to.push(body);
        }
    }
    if hunks.is_empty() {
        return None;
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = if text.is_empty() { !diff.contains("\\ No newline at end of file") } else { text.ends_with('\n') };
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
    // bottom-up, so earlier hunks keep their line numbers
    for h in hunks.iter().rev() {
        let expected = if h.count == 0 { h.start } else { h.start.saturating_sub(1) };
        let matches = |at: usize| at + h.from.len() <= lines.len() && lines[at..at + h.from.len()] == h.from[..];
        let at = if matches(expected) { expected } else { (0..=lines.len().saturating_sub(h.from.len())).find(|&i| matches(i))? };
        lines.splice(at..at + h.from.len(), h.to.iter().cloned());
    }
    let mut out = lines.join(eol);
    if trailing && !out.is_empty() {
        out.push_str(eol);
    }
    Some(out)
}

/// Replay one stored message as ACP updates (`session/load`).
fn history_updates(m: &Message, cwd: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    match m.role {
        Role::User => {
            let text: Vec<&str> = m.parts.iter().filter_map(|p| match p {
                Part::Text { text, synthetic: false } => Some(text.as_str()),
                _ => None,
            }).collect();
            if !text.is_empty() {
                out.push(json!({ "sessionUpdate": "user_message_chunk", "messageId": m.id, "content": { "type": "text", "text": text.join("\n") } }));
            }
            for p in &m.parts {
                if let Part::File(f) = p {
                    if f.mime.starts_with("image/") {
                        out.push(json!({ "sessionUpdate": "user_message_chunk", "messageId": m.id, "content": { "type": "image", "mimeType": f.mime, "data": f.data } }));
                    }
                }
            }
        }
        Role::Assistant => {
            let mut reasoning = 0;
            for p in &m.parts {
                match p {
                    Part::Text { text, .. } if !text.is_empty() => {
                        out.push(json!({ "sessionUpdate": "agent_message_chunk", "messageId": m.id, "content": { "type": "text", "text": text } }))
                    }
                    Part::Reasoning { text, .. } if !text.is_empty() => {
                        out.push(json!({ "sessionUpdate": "agent_thought_chunk", "messageId": format!("{}:reasoning:{reasoning}", m.id), "content": { "type": "text", "text": text } }));
                        reasoning += 1;
                    }
                    Part::Tool { id, name, input, state, .. } => {
                        let mut call = tool_call(id, name, input, cwd);
                        call["sessionUpdate"] = json!("tool_call");
                        out.push(call);
                        match state {
                            ToolState::Completed { output, metadata, .. } => out.push(json!({
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": id,
                                "status": "completed",
                                "locations": locations(name, input, cwd),
                                "content": [{ "type": "content", "content": { "type": "text", "text": output } }],
                                "rawOutput": if metadata.is_null() { json!({}) } else { json!({ "metadata": metadata }) },
                            })),
                            ToolState::Error { error } => out.push(failed_update(id, name, input, error, &Value::Null, cwd)),
                            ToolState::Running => {
                                let mut u = tool_call(id, name, input, cwd);
                                u["sessionUpdate"] = json!("tool_call_update");
                                u["status"] = json!("in_progress");
                                out.push(u);
                            }
                            ToolState::Pending => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_round_trip() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\nk\nl\n";
        let diff = crate::util::unified_diff("x.txt", old, new);
        assert_eq!(apply_unified(old, &diff, false).as_deref(), Some(new));
        assert_eq!(apply_unified(new, &diff, true).as_deref(), Some(old));
        let crlf_old = old.replace('\n', "\r\n");
        assert_eq!(apply_unified(&crlf_old, &diff, false).as_deref(), Some(new.replace('\n', "\r\n").as_str()));
        // pure insertion into an empty file
        let added = crate::util::unified_diff("y.txt", "", "one\ntwo\n");
        assert_eq!(apply_unified("", &added, false).as_deref(), Some("one\ntwo\n"));
        let two = format!("{}{}", crate::util::unified_diff("a", "x\n", "y\n"), crate::util::unified_diff("b", "1\n", "2\n"));
        let parts = split_diff(&two);
        assert_eq!(parts.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn prompt_blocks() {
        let cwd = std::env::temp_dir();
        let blocks = vec![
            json!({ "type": "text", "text": "fix it" }),
            json!({ "type": "text", "text": "hidden", "annotations": { "audience": ["assistant"] } }),
            json!({ "type": "resource", "resource": { "uri": "https://example.com/a", "text": "body" } }),
            json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" }),
        ];
        let p = convert_prompt(&blocks, &cwd).unwrap();
        assert_eq!(p.text, "fix it\n[https://example.com/a]\nbody");
        assert_eq!(p.synthetic, ["hidden"]);
        assert_eq!(p.files.len(), 1);
        assert_eq!(slash("/compact now"), Some(("compact".into(), "now".into())));
        assert_eq!(kind("bash"), "execute");
        let call = tool_call("c1", "shell", &json!({ "command": "ls" }), &cwd);
        assert_eq!(call["title"], "ls");
        assert!(call["rawInput"]["cwd"].is_string());
    }
}
