//! Minimal OpenCode V2 HTTP API and SSE event feed.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::app::{App, AppEvent};

pub struct Options {
    pub hostname: String,
    pub port: u16,
}

pub async fn serve(app: Arc<App>, options: Options) -> Result<()> {
    let listener = TcpListener::bind((options.hostname.as_str(), options.port)).await?;
    let address = listener.local_addr()?;
    eprintln!("opencode server listening on http://{address}");
    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let _ = handle(stream, app).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    body: Value,
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Err(anyhow!("request headers too large"));
        }
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = header.lines();
    let request_line = lines.next().ok_or_else(|| anyhow!("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    Ok(Some(Request { method, path, body }))
}

async fn handle(mut stream: TcpStream, app: Arc<App>) -> Result<()> {
    let Some(request) = read_request(&mut stream).await? else { return Ok(()) };
    if request.method == "OPTIONS" {
        write_response(&mut stream, 204, Value::Null).await?;
        return Ok(());
    }
    if request.path.starts_with("/api/event") || request.path == "/event" {
        return event_feed(stream, app).await;
    }
    let parsed = url::Url::parse(&format!("http://localhost{}", request.path))?;
    let segments: Vec<String> = parsed
        .path_segments()
        .map(|segments| segments.map(String::from).collect())
        .unwrap_or_default();
    let route = route(&app, &request, &segments).await;
    match route {
        Ok(value) => write_response(&mut stream, 200, value).await,
        Err(error) => {
            let value = json!({ "data": null, "error": { "message": format!("{error:#}") } });
            write_response(&mut stream, 400, value).await
        }
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, value: Value) -> Result<()> {
    let body = if value.is_null() { String::new() } else { value.to_string() };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\naccess-control-allow-origin: *\r\naccess-control-allow-headers: *\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn route(app: &Arc<App>, request: &Request, segments: &[String]) -> Result<Value> {
    let method = request.method.as_str();
    let path: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (method, path.as_slice()) {
        ("GET", ["api", "app"] | ["api", "app", "init"]) => Ok(json!({
            "data": {
                "directory": app.directory.to_string_lossy(),
                "worktree": app.worktree.to_string_lossy(),
                "project": app.project,
                "config": serde_json::to_value(&app.config)?,
            }
        })),
        ("GET", ["api", "config"]) => Ok(json!({ "data": serde_json::to_value(&app.config)? })),
        ("GET", ["api", "agent"]) => Ok(json!({
            "data": app.agents().values().map(crate::plugin::agent_json).collect::<Vec<_>>()
        })),
        ("GET", ["api", "provider"]) => Ok(json!({
            "data": app.providers().values().map(|provider| json!({
                "id": provider.id,
                "name": provider.name,
                "models": provider.models.values().map(crate::plugin::model_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()
        })),
        ("GET", ["api", "model"]) => Ok(json!({
            "data": app.providers().values().flat_map(|provider| provider.models.values().map(crate::plugin::model_json)).collect::<Vec<_>>()
        })),
        ("GET", ["api", "session"]) => {
            let sessions = crate::session::list(&app.project)
                .iter()
                .map(crate::plugin::session_json_summary)
                .collect::<Vec<_>>();
            Ok(json!({ "data": sessions }))
        }
        ("POST", ["api", "session"]) => {
            let handle = app.new_session(None);
            handle.save();
            let session = handle.with(|session| session.clone());
            Ok(json!({ "data": crate::plugin::session_json(&session) }))
        }
        ("GET", ["api", "session", id]) => {
            let handle = app.session(id)?;
            let session = handle.with(|session| session.clone());
            Ok(json!({ "data": crate::plugin::session_json(&session) }))
        }
        ("DELETE", ["api", "session", id]) => {
            let removed = crate::session::delete(&app.project, id)?;
            Ok(json!({ "data": removed }))
        }
        ("GET", ["api", "session", id, "message"]) => {
            let handle = app.session(id)?;
            let messages = handle.with(|session| {
                crate::plugin::messages_for_hook(&session.id, &session.messages)
            });
            Ok(json!({ "data": messages }))
        }
        ("POST", ["api", "session", id, "prompt"] | ["api", "session", id, "message"]) => {
            let handle = app.session(id)?;
            let text = request
                .body
                .pointer("/parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| {
                            if part.get("type").and_then(Value::as_str) == Some("text") {
                                part.get("text").and_then(Value::as_str).map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .or_else(|| request.body.get("text").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            if text.trim().is_empty() {
                return Err(anyhow!("prompt text is required"));
            }
            let mut message = crate::session::Message::user_text(text);
            if let Some(id) = request.body.get("messageID").and_then(Value::as_str) {
                message.id = id.to_string();
            }
            let mut opts = crate::app::PromptOptions::default();
            opts.agent = request.body.get("agent").and_then(Value::as_str).map(String::from);
            opts.delivery = request.body.get("delivery").and_then(Value::as_str).map(String::from);
            if let Some(model) = request.body.get("model") {
                let provider = model.get("providerID").and_then(Value::as_str);
                let id = model.get("id").or_else(|| model.get("modelID")).and_then(Value::as_str);
                if let (Some(provider), Some(id)) = (provider, id) {
                    opts.model = Some(format!("{provider}/{id}"));
                }
                opts.variant = model.get("variant").and_then(Value::as_str).map(String::from);
            }
            let id = handle.id();
            app.spawn_prompt(&id, message, opts)?;
            Ok(json!({ "data": { "id": handle.id(), "time": { "created": crate::util::now_ms() } } }))
        }
        ("POST", ["api", "session", id, "interrupt"]) => {
            let handle = app.session(id)?;
            handle.abort();
            Ok(json!({ "data": true }))
        }
        ("POST", ["api", "session", id, "compact"]) => {
            let app = app.clone();
            let session = (*id).to_string();
            tokio::spawn(async move {
                let _ = crate::engine::compact_session(app, &session).await;
            });
            Ok(json!({ "data": null }))
        }
        ("POST", ["api", "session", id, "fork"]) => {
            let handle = app.session(id)?;
            let forked = handle.with(|session| crate::session::fork(session));
            crate::session::save(&forked)?;
            Ok(json!({ "data": crate::plugin::session_json(&forked) }))
        }
        ("GET", ["api", "session", id, "diff"]) => {
            let handle = app.session(id)?;
            let from = handle.with(|session| {
                session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == crate::session::Role::User)
                    .and_then(|message| message.snapshot.clone())
            });
            let diffs = from
                .as_deref()
                .and_then(|from| crate::snapshot::diff(&app.worktree, from, None).ok())
                .unwrap_or_default();
            Ok(json!({ "data": diffs }))
        }
        ("POST", ["api", "session", id, "revert"]) => {
            let handle = app.session(id)?;
            let message_id = request.body.get("messageID").and_then(Value::as_str).unwrap_or_default();
            let snapshot = handle.with(|session| {
                session
                    .messages
                    .iter()
                    .find(|message| message.id == message_id)
                    .and_then(|message| message.snapshot.clone())
            });
            if let Some(snapshot) = snapshot {
                crate::snapshot::restore(&app.worktree, &snapshot)?;
            }
            handle.with(|session| {
                if let Some(index) = session.messages.iter().position(|message| message.id == message_id) {
                    session.messages.truncate(index + 1);
                }
            });
            handle.save();
            Ok(json!({ "data": null }))
        }
        ("GET", ["api", "session", id, "log"]) => {
            let after = request.body.get("after").and_then(Value::as_u64);
            let events = crate::event_store::read(Some(id), after, None);
            Ok(json!({ "data": events }))
        }
        ("GET", ["api", "artifact"]) => Ok(json!({ "data": crate::artifact::list()? })),
        ("POST", ["api", "artifact"]) => Ok(json!({ "data": crate::artifact::create(&request.body)? })),
        ("GET", ["api", "artifact", id]) => Ok(json!({ "data": crate::artifact::get(id)? })),
        ("PATCH" | "PUT", ["api", "artifact", id]) => {
            Ok(json!({ "data": crate::artifact::update(id, &request.body)? }))
        }
        ("DELETE", ["api", "artifact", id]) => Ok(json!({ "data": crate::artifact::delete(id)? })),
        ("GET", ["api", "experimental", "session", id, "terminal"]) => {
            app.session(id)?;
            let terminals = crate::plugin::pty::manager()
                .list()
                .into_iter()
                .filter(|terminal| terminal["sessionID"].as_str() == Some(*id))
                .collect::<Vec<_>>();
            Ok(json!({ "data": terminals }))
        }
        ("POST", ["api", "experimental", "session", id, "terminal"]) => {
            app.session(id)?;
            let command = request.body.get("command").and_then(Value::as_str).unwrap_or_default();
            let args = request
                .body
                .get("args")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            let cwd = request
                .body
                .get("cwd")
                .and_then(Value::as_str)
                .map(|cwd| crate::util::resolve(&app.directory, cwd))
                .unwrap_or_else(|| app.directory.clone());
            let env = request
                .body
                .get("env")
                .and_then(Value::as_object)
                .map(|map| map.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect())
                .unwrap_or_default();
            let size = request.body.get("size");
            let cols = size.and_then(|size| size.get("cols")).and_then(Value::as_u64).unwrap_or(120) as u16;
            let rows = size.and_then(|size| size.get("rows")).and_then(Value::as_u64).unwrap_or(40) as u16;
            let terminal = crate::plugin::pty::manager().create(id, command, &args, &cwd, &env, cols, rows)?;
            Ok(json!({ "data": terminal }))
        }
        ("GET", ["api", "experimental", "session", id, "terminal", "read"]) => {
            app.session(id)?;
            Ok(json!({ "data": crate::plugin::pty::manager().read(id) }))
        }
        ("POST", ["api", "experimental", "session", id, "terminal", "write"]) => {
            app.session(id)?;
            let data = request.body.get("data").and_then(Value::as_str).unwrap_or_default();
            crate::plugin::pty::manager().write(id, data)?;
            Ok(json!({ "data": null }))
        }
        ("POST", ["api", "experimental", "session", id, "terminal", "resize"]) => {
            app.session(id)?;
            let size = request.body.get("size").cloned().unwrap_or(Value::Null);
            let cols = size.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
            let rows = size.get("rows").and_then(Value::as_u64).unwrap_or(40) as u16;
            crate::plugin::pty::manager().resize(id, cols, rows)?;
            Ok(json!({ "data": null }))
        }
        ("DELETE", ["api", "experimental", "session", id, "terminal"]) => {
            app.session(id)?;
            crate::plugin::pty::manager().kill(id);
            Ok(json!({ "data": null }))
        }
        ("POST", ["api", "rpc", rpc, method]) => {
            let result = app.plugins.rpc_call(rpc, method, request.body.clone()).await?;
            Ok(json!({ "data": result }))
        }
        ("GET", ["api", "permission"]) => Ok(json!({ "data": [] })),
        ("GET", ["api", "form"]) => Ok(json!({ "data": [] })),
        _ => Err(anyhow!("not found: {} {}", request.method, request.path)),
    }
}

async fn event_feed(mut stream: TcpStream, app: Arc<App>) -> Result<()> {
    let mut events = app.subscribe();
    let mut plugin_events = app.plugins.subscribe();
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\naccess-control-allow-origin: *\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    let connected = json!({ "id": crate::util::id("evt"), "type": "server.connected", "properties": {} });
    stream.write_all(format!("data: {connected}\n\n").as_bytes()).await?;
    loop {
        let value = tokio::select! {
            event = events.recv() => match event {
                Ok(event) => crate::app::plugin_event(&event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            event = plugin_events.recv() => match event {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
        };
        let payload = format!("data: {value}\n\n");
        if stream.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn event_type(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::TextDelta { .. } => "session.text.delta",
        AppEvent::ReasoningDelta { .. } => "session.reasoning.delta",
        _ => "message.updated",
    }
}
