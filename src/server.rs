//! Minimal OpenCode V2 HTTP API and SSE event feed.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::app::{App, AppEvent};

pub struct Options {
    pub hostname: String,
    pub port: u16,
    pub password: Option<String>,
}

pub async fn serve(app: Arc<App>, options: Options) -> Result<()> {
    let listener = TcpListener::bind((options.hostname.as_str(), options.port)).await?;
    let address = listener.local_addr()?;
    eprintln!("opencode server listening on http://{address}");
    let pairing: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        let password = options.password.clone();
        let pairing = pairing.clone();
        let port = address.port();
        tokio::spawn(async move {
            let _ = handle(stream, app, password, pairing, port).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    cookie: Option<String>,
    accept: Option<String>,
    websocket_key: Option<String>,
    upgrade: bool,
    body: Value,
    raw_body: Vec<u8>,
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
    let mut authorization = None;
    let mut cookie = None;
    let mut accept = None;
    let mut websocket_key = None;
    let mut upgrade = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("cookie") {
                cookie = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("accept") {
                accept = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("sec-websocket-key") {
                websocket_key = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("upgrade") && value.trim().eq_ignore_ascii_case("websocket") {
                upgrade = true;
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
    let raw_body = body;
    let body = if raw_body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw_body).unwrap_or(Value::Null)
    };
    Ok(Some(Request { method, path, authorization, cookie, accept, websocket_key, upgrade, body, raw_body }))
}

fn authorized(request: &Request, password: &str, port: u16) -> bool {
    use base64::Engine;
    if let Some(header) = &request.authorization {
        if let Some(token) = header.strip_prefix("Basic ").or_else(|| header.strip_prefix("basic ")) {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(token.trim()) {
                if let Ok(decoded) = String::from_utf8(decoded) {
                    if decoded.split_once(':').is_some_and(|(username, candidate)| username == "opencode" && (candidate == password || verify_session(candidate, password))) {
                        return true;
                    }
                }
            }
        }
    }
    let Some(cookie) = &request.cookie else { return false };
    let name = format!("opencode_session_{port}=");
    cookie
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&name))
        .is_some_and(|token| verify_session(token, password))
}

fn session_cookie(port: u16) -> String {
    format!("opencode_session_{port}")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha2::Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    let mut outer = Vec::with_capacity(64 + 32);
    for byte in &block {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(data);
    outer.extend_from_slice(&sha2::Sha256::digest(&inner));
    sha2::Sha256::digest(&outer).into()
}

fn session_token(password: &str, expires: i64) -> String {
    use base64::Engine;
    let payload = expires.to_string();
    let signature = hmac_sha256(password.as_bytes(), format!("opencode-session-v1{payload}").as_bytes());
    format!("{payload}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature))
}

fn verify_session(token: &str, password: &str) -> bool {
    use base64::Engine;
    let Some((expires, signature)) = token.split_once('.') else { return false };
    let Ok(expires) = expires.parse::<i64>() else { return false };
    if expires * 1000 <= crate::util::now_ms() {
        return false;
    }
    let expected = session_token(password, expires);
    let expected = expected.split_once('.').map(|(_, signature)| signature).unwrap_or_default();
    if expected.len() != signature.len() {
        return false;
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(expected).ok()
        == base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signature).ok()
}

fn pairing_code() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn issue_pairing(pairing: &Mutex<HashMap<String, i64>>) -> Value {
    let code = pairing_code();
    pairing.lock().unwrap().insert(code.clone(), crate::util::now_ms() + 300_000);
    json!({ "code": code, "expires_in": 300 })
}

fn consume_pairing(pairing: &Mutex<HashMap<String, i64>>, code: &str) -> bool {
    let now = crate::util::now_ms();
    let mut pairing = pairing.lock().unwrap();
    pairing.retain(|_, expires| *expires > now);
    pairing.remove(code).is_some()
}

async fn handle(
    mut stream: TcpStream,
    app: Arc<App>,
    password: Option<String>,
    pairing: Arc<Mutex<HashMap<String, i64>>>,
    port: u16,
) -> Result<()> {
    let Some(request) = read_request(&mut stream).await? else { return Ok(()) };
    if request.method == "OPTIONS" {
        write_response(&mut stream, 204, Value::Null).await?;
        return Ok(());
    }
    let password = password.filter(|password| !password.is_empty());
    if let Some(code) = request.path.strip_prefix("/auth/connect/") {
        let valid = password.is_some() && consume_pairing(&pairing, code);
        if !valid {
            let body = json!({ "error": { "message": "This pairing link expired or was already used. Run `opencode pair` to get a new one." } });
            write_status(&mut stream, 401, "Unauthorized", body, None).await?;
            return Ok(());
        }
        let token = session_token(password.as_deref().unwrap_or_default(), (crate::util::now_ms() / 1000) + 30 * 24 * 60 * 60);
        if request.accept.as_deref().is_some_and(|accept| accept.contains("text/html")) {
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: /\r\nSet-Cookie: {name}={token}; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                name = session_cookie(port),
            );
            stream.write_all(response.as_bytes()).await?;
            stream.flush().await?;
            return Ok(());
        }
        return write_response(&mut stream, 200, json!({ "token": token })).await;
    }
    if let Some(password) = password {
        if !authorized(&request, &password, port) {
            let body = json!({ "error": { "message": "Authentication required" } });
            write_status(&mut stream, 401, "Unauthorized", body, Some("Basic realm=\"Secure Area\"")).await?;
            return Ok(());
        }
    }
    if request.method == "POST" && request.path == "/api/pair" {
        return write_response(&mut stream, 200, issue_pairing(&pairing)).await;
    }
    if request.upgrade {
        if let Some(id) = pty_connect_id(&request.path) {
            return pty_socket(stream, app, &id, request.websocket_key.as_deref()).await;
        }
        let body = json!({ "error": { "message": "WebSocket route not found" } });
        return write_status(&mut stream, 404, "Not Found", body, None).await;
    }
    if request.path.starts_with("/api/event") || request.path == "/event" {
        return event_feed(stream, app).await;
    }
    let parsed = url::Url::parse(&format!("http://localhost{}", request.path))?;
    if request.method == "GET" {
        if let Some(raw) = parsed.path().strip_prefix("/api/fs/read/") {
            let path = decode_path(raw);
            return fs_read(stream, &app, &path).await;
        }
    }
    if request.method == "POST" && parsed.path() == "/api/experimental/fs/write" {
        let path = query_value(&parsed, "path").unwrap_or_default();
        return fs_write(stream, &request, &path).await;
    }
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
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    write_status(stream, status, reason, value, None).await
}

async fn write_status(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    value: Value,
    extra: Option<&str>,
) -> Result<()> {
    let body = if value.is_null() { String::new() } else { value.to_string() };
    let challenge = extra.map(|extra| format!("{extra}\r\n")).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\naccess-control-allow-origin: *\r\naccess-control-allow-headers: *\r\n{challenge}connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn decode_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn query_value(parsed: &url::Url, key: &str) -> Option<String> {
    parsed
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

fn fs_target(root: &std::path::Path, path: &str) -> std::path::PathBuf {
    if path.is_empty() {
        return root.to_path_buf();
    }
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

async fn fs_read(mut stream: TcpStream, app: &App, path: &str) -> Result<()> {
    let root = std::fs::canonicalize(&app.directory).unwrap_or_else(|_| app.directory.clone());
    let requested = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        app.directory.join(path)
    };
    let resolved = match std::fs::canonicalize(&requested) {
        Ok(path) if path.starts_with(&root) => path,
        _ => {
            write_status(&mut stream, 404, "Not Found", json!({ "error": { "message": "File not found" } }), None).await?;
            return Ok(());
        }
    };
    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(_) => {
            write_status(&mut stream, 404, "Not Found", json!({ "error": { "message": "File not found" } }), None).await?;
            return Ok(());
        }
    };
    let content_type = mime_guess::from_path(&resolved).first_or_octet_stream().to_string();
    write_bytes(&mut stream, 200, "OK", &content_type, &bytes).await
}

async fn fs_write(mut stream: TcpStream, request: &Request, path: &str) -> Result<()> {
    if path.is_empty() {
        write_status(&mut stream, 400, "Bad Request", json!({ "error": { "message": "path is required" } }), None).await?;
        return Ok(());
    }
    let target = std::path::PathBuf::from(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, &request.raw_body)?;
    write_response(&mut stream, 200, json!({ "data": { "path": target.to_string_lossy() } })).await
}

async fn write_bytes(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\naccess-control-allow-origin: *\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
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
        ("GET", ["api", "fs", "list"]) => {
            let parsed = url::Url::parse(&format!("http://localhost{}", request.path))?;
            let path = query_value(&parsed, "path").unwrap_or_default();
            let root = std::fs::canonicalize(&app.directory).unwrap_or_else(|_| app.directory.clone());
            let target = fs_target(&root, &path);
            let mut entries = Vec::new();
            if let Ok(read) = std::fs::read_dir(&target) {
                for entry in read.flatten() {
                    let path = entry.path();
                    let relative = path.strip_prefix(&root).unwrap_or(&path);
                    entries.push(json!({
                        "path": relative.to_string_lossy().replace('\\', "/"),
                        "type": if path.is_dir() { "directory" } else { "file" },
                    }));
                }
            }
            entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            Ok(json!({ "data": entries }))
        }
        ("GET", ["api", "fs", "find"]) => {
            let parsed = url::Url::parse(&format!("http://localhost{}", request.path))?;
            let query = query_value(&parsed, "query").unwrap_or_default().to_lowercase();
            let kind = query_value(&parsed, "type");
            let limit = query_value(&parsed, "limit").and_then(|value| value.parse::<usize>().ok()).unwrap_or(100);
            let root = std::fs::canonicalize(&app.directory).unwrap_or_else(|_| app.directory.clone());
            let mut entries = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                if entries.len() >= limit {
                    break;
                }
                let Ok(read) = std::fs::read_dir(&dir) else { continue };
                for entry in read.flatten() {
                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let relative = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                    if is_dir {
                        stack.push(path.clone());
                    }
                    if !query.is_empty() && !relative.to_lowercase().contains(&query) {
                        continue;
                    }
                    if kind.as_deref().is_some_and(|kind| kind != if is_dir { "directory" } else { "file" }) {
                        continue;
                    }
                    entries.push(json!({ "path": relative, "type": if is_dir { "directory" } else { "file" } }));
                    if entries.len() >= limit {
                        break;
                    }
                }
            }
            entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            Ok(json!({ "data": entries }))
        }
        ("POST", ["api", "experimental", "fs", "watch"]) | ("DELETE", ["api", "experimental", "fs", "watch"]) => {
            Ok(Value::Null)
        }
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
        ("GET", ["api", "session", "active"]) => {
            let active = app
                .session_statuses()
                .into_iter()
                .filter(|(_, busy, _)| *busy)
                .map(|(id, _, retry)| {
                    let value = match retry {
                        Some(retry) => json!({ "status": "retry", "retry": retry }),
                        None => json!({ "status": "busy" }),
                    };
                    (id, value)
                })
                .collect::<serde_json::Map<String, Value>>();
            Ok(json!({ "data": active }))
        }
        ("GET", ["api", "experimental", "session", "stats"]) => Ok(json!({ "data": session_stats(app) })),
        ("GET", ["api", "experimental", "session", id, "export"]) => {
            let handle = app.session(id)?;
            let session = handle.with(|session| session.clone());
            Ok(json!({
                "data": {
                    "info": crate::plugin::session_json(&session),
                    "messages": crate::plugin::session_messages_json(&session.messages),
                }
            }))
        }
        ("POST", ["api", "experimental", "session", "import"]) => {
            let info = request.body.get("info").cloned().unwrap_or_else(|| request.body.clone());
            let id = info.get("id").and_then(Value::as_str).map(String::from);
            let handle = match id.and_then(|id| app.session(&id).ok()) {
                Some(handle) => handle,
                None => {
                    let handle = app.new_session(info.get("parentID").and_then(Value::as_str).map(String::from));
                    handle.with(|session| {
                        if let Some(id) = info.get("id").and_then(Value::as_str) {
                            session.id = id.to_string();
                        }
                        if let Some(title) = info.get("title").and_then(Value::as_str) {
                            session.title = title.to_string();
                        }
                    });
                    handle
                }
            };
            if let Some(messages) = request.body.get("messages").and_then(Value::as_array) {
                let imported: Vec<crate::session::Message> = messages
                    .iter()
                    .filter_map(|message| serde_json::from_value(message.clone()).ok())
                    .collect();
                handle.with(|session| session.messages.extend(imported));
            }
            handle.save();
            let session = handle.with(|session| session.clone());
            Ok(json!({ "data": crate::plugin::session_json(&session) }))
        }
        ("GET", ["api", "experimental", "session", id, "log"]) => {
            session_call(app, "session.log", id, json!({})).await
        }
        ("POST", ["api", "experimental", "session", id, "wait"]) => {
            session_call(app, "session.wait", id, request.body.clone()).await
        }
        ("POST", ["api", "experimental", "session", id, "skill"]) => {
            session_call(app, "session.skill", id, request.body.clone()).await
        }
        ("PATCH", ["api", "experimental", "config"]) => {
            let mut config = serde_json::to_value(&app.config)?;
            if let (Some(target), Some(patch)) = (config.as_object_mut(), request.body.as_object()) {
                for (key, value) in patch {
                    target.insert(key.clone(), value.clone());
                }
            }
            Ok(json!({ "data": config }))
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
            let messages = handle.with(|session| crate::plugin::session_messages_json(&session.messages));
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
        ("GET", ["api", "pty"]) => Ok(json!({ "data": crate::plugin::pty::manager().list() })),
        ("POST", ["api", "pty"]) => {
            let id = request
                .body
                .get("id")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| crate::util::id("pty"));
            let command = request.body.get("command").and_then(Value::as_str).unwrap_or_default();
            let args = request
                .body
                .get("args")
                .and_then(Value::as_array)
                .map(|args| args.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            let cwd = request
                .body
                .get("cwd")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| app.directory.clone());
            let env = request
                .body
                .get("env")
                .and_then(Value::as_object)
                .map(|env| env.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect())
                .unwrap_or_default();
            let info = crate::plugin::pty::manager().create(&id, command, &args, &cwd, &env, 120, 40)?;
            Ok(json!({ "data": info }))
        }
        ("GET", ["api", "pty", id]) => Ok(json!({ "data": crate::plugin::pty::manager().read(id) })),
        ("PUT" | "PATCH", ["api", "pty", id]) => {
            if let Some(size) = request.body.get("size") {
                let cols = size.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
                let rows = size.get("rows").and_then(Value::as_u64).unwrap_or(40) as u16;
                crate::plugin::pty::manager().resize(id, cols, rows)?;
            }
            Ok(json!({ "data": crate::plugin::pty::manager().read(id) }))
        }
        ("DELETE", ["api", "pty", id]) => {
            crate::plugin::pty::manager().kill(id);
            Ok(json!({ "data": null }))
        }
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
        ("GET", ["api", "vcs"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            Ok(json!({ "data": crate::vcs::dispatch("vcs.info", &input)? }))
        }
        ("GET", ["api", "vcs", "base"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            Ok(json!({ "data": crate::vcs::dispatch("vcs.base", &input)? }))
        }
        ("GET", ["api", "vcs", "status"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            Ok(json!({ "data": crate::vcs::dispatch("vcs.status", &input)? }))
        }
        ("GET", ["api", "vcs", "branch"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            Ok(json!({ "data": crate::vcs::dispatch("vcs.branches", &input)? }))
        }
        ("GET", ["api", "vcs", "diff"]) => {
            let input = json!({
                "directory": app.directory.to_string_lossy(),
                "mode": "working",
                "context": 3,
            });
            Ok(json!({ "data": crate::vcs::dispatch("vcs.diff", &input)? }))
        }
        ("POST", ["api", "vcs", "init"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            crate::vcs::dispatch("vcs.init", &input)?;
            Ok(json!({ "data": null }))
        }
        ("GET", ["api", "worktree"]) => {
            let input = json!({ "directory": app.directory.to_string_lossy() });
            Ok(json!({ "data": crate::vcs::dispatch("worktree.list", &input)? }))
        }
        ("POST", ["api", "worktree"]) => {
            let mut input = request.body.clone();
            input["sourceDirectory"] = json!(app.directory.to_string_lossy());
            Ok(json!({ "data": crate::vcs::dispatch("worktree.create", &input)? }))
        }
        ("DELETE", ["api", "worktree"]) => {
            let input = request.body.clone();
            crate::vcs::dispatch("worktree.remove", &input)?;
            Ok(json!({ "data": null }))
        }
        ("POST", ["api", "rpc", rpc, method]) => {
            let result = app.plugins.rpc_call(rpc, method, request.body.clone()).await?;
            Ok(json!({ "data": result }))
        }
        ("GET", ["api", "info"]) => Ok(json!({
            "version": crate::UPSTREAM_VERSION,
            "pid": std::process::id(),
            "hostname": std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok(),
            "urls": [],
            "paths": { "tmp": crate::paths::get().tmp.to_string_lossy() },
            "capabilities": { "persistentPty": true },
        })),
        ("GET", ["api", "location"]) => Ok(json!({
            "directory": app.directory.to_string_lossy(),
            "worktree": app.worktree.to_string_lossy(),
            "projectID": app.project,
            "vcs": if app.is_git { "git" } else { "none" },
        })),
        ("POST", ["api", "location", "reload"]) => Ok(Value::Null),
        ("GET", ["api", "config", "shell"]) => {
            let shell = crate::tools::shell::detect();
            Ok(json!({
                "shell": {
                    "program": shell.program.to_string_lossy(),
                    "args": shell.args,
                },
            }))
        }
        ("GET", ["api", "command"]) => Ok(json!({
            "data": app.commands().values().map(|command| json!({
                "name": command.name,
                "description": command.description,
                "agent": command.agent,
                "model": command.model,
                "template": command.template,
            })).collect::<Vec<_>>(),
        })),
        ("GET", ["api", "skill"]) => Ok(json!({
            "data": app.skills().iter().map(|skill| json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
            })).collect::<Vec<_>>(),
        })),
        ("GET", ["api", "mcp"]) => {
            let status = app.mcp.status().await;
            Ok(json!({
                "data": status.into_iter().map(|(name, status)| json!({
                    "name": name,
                    "status": match status {
                        crate::mcp::Status::Connected { tools } => json!({ "status": "connected", "tools": tools }),
                        crate::mcp::Status::Disabled => json!({ "status": "disabled" }),
                        crate::mcp::Status::Failed(error) => json!({ "status": "error", "error": error }),
                    }
                })).collect::<Vec<_>>(),
            }))
        }
        ("GET", ["api", "mcp", "resource"]) => Ok(json!({ "data": app.mcp.resource_catalog(None).await? })),
        ("PUT", ["api", "experimental", "mcp", name]) => {
            let config: crate::config::McpConfig = serde_json::from_value(request.body.clone())?;
            app.mcp.set_server(name, config);
            Ok(Value::Null)
        }
        ("DELETE", ["api", "experimental", "mcp", name]) => {
            app.mcp.remove_server(name);
            Ok(Value::Null)
        }
        ("POST", ["api", "experimental", "mcp", name, "connect"]) => {
            let status = app.mcp.connect_server(name).await?;
            Ok(json!({ "data": mcp_status_json(&status) }))
        }
        ("POST", ["api", "experimental", "mcp", name, "disconnect"]) => {
            app.mcp.disconnect_server(name);
            Ok(Value::Null)
        }
        ("GET", ["api", "plugin"]) => Ok(json!({ "data": app.plugins.list() })),
        ("GET", ["api", "reference"]) => Ok(json!({ "data": [] })),
        ("GET", ["api", "agent", agent_id]) => {
            let agent = app.agents().get(*agent_id).cloned().ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;
            Ok(json!({ "data": crate::plugin::agent_json(&agent) }))
        }
        ("GET", ["api", "provider", provider_id]) => {
            let provider = app.providers().get(*provider_id).cloned().ok_or_else(|| anyhow!("provider not found: {provider_id}"))?;
            Ok(json!({
                "data": {
                    "id": provider.id,
                    "name": provider.name,
                    "models": provider.models.values().map(crate::plugin::model_json).collect::<Vec<_>>(),
                }
            }))
        }
        ("GET", ["api", "model", "default"]) => {
            let model = app
                .default_model()
                .and_then(|reference| crate::models::parse_ref(&reference))
                .and_then(|(provider_id, id, variant)| {
                    app.providers()
                        .get(&provider_id)
                        .and_then(|provider| provider.models.get(&id))
                        .map(|model| (provider_id, id, variant, model.clone()))
                })
                .map(|(provider_id, id, variant, model)| {
                    let mut value = crate::plugin::model_json(&model);
                    value["providerID"] = json!(provider_id);
                    value["id"] = json!(id);
                    value["variant"] = json!(variant);
                    value
                });
            Ok(json!({ "data": model }))
        }
        ("GET", ["api", "permission", "request"]) => Ok(json!({
            "data": app.pending_permissions().iter().map(permission_json).collect::<Vec<_>>(),
        })),
        ("GET", ["api", "permission", "saved"]) => Ok(json!({ "data": [] })),
        ("GET", ["api", "experimental", "job"]) => Ok(json!({ "data": [] })),
        ("GET", ["api", "experimental", "usage", "limits"]) => Ok(json!({ "data": null })),
        ("GET", ["api", "experimental", "migration", "v1"]) => Ok(json!({ "data": { "status": "idle" } })),
        ("GET", ["api", "experimental", "artifacts", "providers"]) => Ok(json!({
            "data": [{ "id": "local", "name": "Local" }],
        })),
        ("GET", ["api", "experimental", "artifacts"]) => Ok(json!({ "data": crate::artifact::list()? })),
        ("POST", ["api", "experimental", "artifacts"]) => Ok(json!({ "data": crate::artifact::create(&request.body)? })),
        ("GET", ["api", "experimental", "artifacts", artifact_id]) => {
            Ok(json!({ "data": crate::artifact::get(artifact_id)? }))
        }
        ("PATCH" | "PUT", ["api", "experimental", "artifacts", artifact_id]) => {
            Ok(json!({ "data": crate::artifact::update(artifact_id, &request.body)? }))
        }
        ("DELETE", ["api", "experimental", "artifacts", artifact_id]) => {
            Ok(json!({ "data": crate::artifact::delete(artifact_id)? }))
        }
        ("GET", ["api", "experimental", "schedule"]) => Ok(json!({
            "data": app.schedules.list(None),
        })),
        ("POST", ["api", "experimental", "schedule"]) => {
            let session = request.body.get("sessionID").and_then(Value::as_str).unwrap_or_default();
            if session.is_empty() {
                return Err(anyhow!("schedule.create requires sessionID"));
            }
            app.session(session)?;
            Ok(json!({ "data": app.schedules.create(session, &app.directory, &request.body).await? }))
        }
        ("GET", ["api", "experimental", "schedule", schedule_id]) => {
            Ok(json!({ "data": app.schedules.get(schedule_id)? }))
        }
        ("POST", ["api", "experimental", "schedule", schedule_id, "pause"]) => {
            Ok(json!({ "data": app.schedules.pause(schedule_id)? }))
        }
        ("POST", ["api", "experimental", "schedule", schedule_id, "resume"]) => {
            Ok(json!({ "data": app.schedules.resume(schedule_id)? }))
        }
        ("DELETE", ["api", "experimental", "schedule", schedule_id]) => {
            Ok(json!({ "data": app.schedules.remove(schedule_id)? }))
        }
        ("GET", ["api", "form"]) => Ok(json!({ "data": app.forms.all().iter().map(crate::plugin::form_info_json).collect::<Vec<_>>() })),
        ("GET", ["api", "debug", "location"]) => Ok(json!({
            "directory": app.directory.to_string_lossy(),
            "worktree": app.worktree.to_string_lossy(),
            "projectID": app.project,
        })),
        ("DELETE", ["api", "debug", "location"]) => Ok(Value::Null),
        ("GET", ["api", "session", id, "context"]) => session_call(app, "session.context", id, json!({})).await,
        ("GET", ["api", "session", id, "status"]) => session_call(app, "session.status", id, json!({})).await,
        ("GET", ["api", "session", id, "children"]) => session_call(app, "session.children", id, json!({})).await,
        ("GET", ["api", "session", id, "todo"]) => session_call(app, "session.todo", id, json!({})).await,
        ("GET", ["api", "session", id, "inbox"]) => session_call(app, "session.inbox.list", id, json!({})).await,
        ("DELETE", ["api", "session", id, "inbox", inbox_id]) => {
            session_call(app, "session.inbox.cancel", id, json!({ "inboxID": inbox_id })).await
        }
        ("PATCH", ["api", "session", id, "inbox", inbox_id]) => {
            let mut body = request.body.clone();
            body["inboxID"] = json!(inbox_id);
            session_call(app, "session.inbox.update", id, body).await
        }
        ("POST", ["api", "session", id, "agent"]) => session_call(app, "session.switchAgent", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "model"]) => session_call(app, "session.switchModel", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "command"]) => session_call(app, "session.command", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "skill"]) => session_call(app, "session.skill", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "synthetic"]) => session_call(app, "session.synthetic", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "shell"]) => session_call(app, "session.shell", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "wait"]) => session_call(app, "session.wait", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "move"]) => session_call(app, "session.move", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "background"]) => session_call(app, "session.background", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "generate"]) => session_call(app, "session.generate", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "revert", "stage"]) => session_call(app, "session.revert.stage", id, request.body.clone()).await,
        ("DELETE", ["api", "session", id, "revert"]) => session_call(app, "session.revert.clear", id, json!({})).await,
        ("POST", ["api", "session", id, "revert", "commit"]) => session_call(app, "session.revert.commit", id, request.body.clone()).await,
        ("PATCH", ["api", "session", id]) => session_call(app, "session.update", id, request.body.clone()).await,
        ("GET", ["api", "session", id, "message", message_id]) => {
            session_call(app, "session.message.get", id, json!({ "messageID": message_id })).await
        }
        ("GET", ["api", "session", id, "form"]) => session_call(app, "session.form.list", id, json!({})).await,
        ("POST", ["api", "session", id, "form"]) => session_call(app, "session.form.create", id, request.body.clone()).await,
        ("GET", ["api", "session", id, "form", form_id]) => {
            session_call(app, "session.form.get", id, json!({ "formID": form_id })).await
        }
        ("POST", ["api", "session", id, "form", form_id, "reply"]) => {
            let mut body = request.body.clone();
            if !body.get("answer").is_some() {
                body["answer"] = body.get("values").cloned().unwrap_or_else(|| json!({}));
            }
            body["formID"] = json!(form_id);
            session_call(app, "session.form.reply", id, body).await
        }
        ("DELETE", ["api", "session", id, "form", form_id]) => {
            session_call(app, "session.form.cancel", id, json!({ "formID": form_id })).await
        }
        ("PUT", ["api", "session", id, "environment"]) => session_call(app, "session.environment", id, request.body.clone()).await,
        ("POST", ["api", "session", id, "view"]) => session_call(app, "session.view", id, request.body.clone()).await,
        ("GET", ["api", "session", id, "permission"]) => Ok(json!({
            "data": app.pending_permissions().iter().filter(|request| request.session_id.as_str() == *id).map(permission_json).collect::<Vec<_>>(),
        })),
        ("GET", ["api", "session", id, "permission", request_id]) => {
            let request = app
                .pending_permissions()
                .into_iter()
                .find(|request| request.session_id.as_str() == *id && request.id.as_str() == *request_id)
                .ok_or_else(|| anyhow!("permission not found: {request_id}"))?;
            Ok(json!({ "data": permission_json(&request) }))
        }
        ("POST", ["api", "session", id, "permission", request_id, "reply"]) => {
            let permission = app
                .pending_permissions()
                .into_iter()
                .find(|permission| permission.session_id.as_str() == *id && permission.id.as_str() == *request_id)
                .ok_or_else(|| anyhow!("permission not found: {request_id}"))?;
            let reply = match request.body.get("decision").or_else(|| request.body.get("reply")).and_then(Value::as_str) {
                Some("once") | Some("allow") => crate::permission::Reply::Once,
                Some("always") => crate::permission::Reply::Always,
                Some("reject") | Some("deny") => crate::permission::Reply::Reject,
                _ => return Err(anyhow!("permission reply must be once, always or reject")),
            };
            app.reply_permission(&permission.id, reply);
            Ok(Value::Null)
        }
        _ => Err(anyhow!("not found: {} {}", request.method, request.path)),
    }
}

fn mcp_status_json(status: &crate::mcp::Status) -> Value {
    match status {
        crate::mcp::Status::Connected { tools } => json!({ "status": "connected", "tools": tools }),
        crate::mcp::Status::Disabled => json!({ "status": "disabled" }),
        crate::mcp::Status::Failed(error) => json!({ "status": "error", "error": error }),
    }
}

fn session_stats(app: &App) -> Value {
    let sessions = crate::session::list(&app.project);
    let mut tokens = crate::session::Usage::default();
    let mut cost = 0.0;
    let mut days: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut subagents = 0;
    for session in &sessions {
        tokens.add(&session.tokens);
        cost += session.cost;
        if session.parent_id.is_some() {
            subagents += 1;
        }
        let day = (session.time_updated / 86_400_000).to_string();
        *days.entry(day).or_default() += 1;
    }
    let now = crate::util::now_ms();
    json!({
        "range": { "from": 0, "to": now },
        "sessions": sessions.len(),
        "subagents": subagents,
        "prompts": sessions.len(),
        "steps": 0,
        "tokens": {
            "input": tokens.input,
            "output": tokens.output,
            "reasoning": tokens.reasoning,
            "cache": { "read": tokens.cache_read, "write": tokens.cache_write },
        },
        "cost": cost,
        "tools": { "mode": "summary", "totals": { "calls": 0, "succeeded": 0, "failed": 0, "unfinished": 0 } },
        "activeDays": days.len(),
        "streak": 0,
        "activity": days.into_iter().map(|(date, steps)| json!({ "date": date, "steps": steps })).collect::<Vec<_>>(),
        "models": [],
    })
}

async fn session_call(app: &Arc<App>, method: &str, id: &str, mut body: Value) -> Result<Value> {
    if !body.is_object() {
        body = json!({});
    }
    body["sessionID"] = json!(id);
    Ok(client_response(crate::plugin::client_call(app, method, vec![body]).await?))
}

fn client_response(value: Value) -> Value {
    if value.is_null() {
        json!({ "data": null })
    } else if value.get("data").is_some() {
        value
    } else {
        json!({ "data": value })
    }
}

fn permission_json(request: &crate::permission::Request) -> Value {
    json!({
        "id": request.id,
        "sessionID": request.session_id,
        "toolCallID": request.tool_call_id,
        "action": request.action,
        "resources": request.resources,
        "title": request.title,
        "metadata": request.metadata,
        "save": request.save,
    })
}

fn pty_connect_id(path: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://localhost{path}")).ok()?;
    let segments: Vec<&str> = url.path_segments()?.collect();
    match segments.as_slice() {
        ["api", "pty", id, "connect"] => Some((*id).to_string()),
        ["api", "session", id, "pty"] => Some((*id).to_string()),
        _ => None,
    }
}

fn websocket_accept(key: &str) -> String {
    use base64::Engine;
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.digest().bytes())
}

async fn pty_socket(mut stream: TcpStream, app: Arc<App>, id: &str, key: Option<&str>) -> Result<()> {
    let Some(key) = key else {
        return write_status(&mut stream, 400, "Bad Request", json!({ "error": { "message": "Missing Sec-WebSocket-Key" } }), None).await;
    };
    if !crate::plugin::pty::manager().exists(id) {
        return write_status(&mut stream, 404, "Not Found", json!({ "error": { "message": format!("PTY session not found: {id}") } }), None).await;
    }
    let accept = websocket_accept(key);
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await?;
    stream.flush().await?;
    let (mut reader, mut writer) = stream.into_split();
    // Replay the current screen so a reconnecting client sees the terminal immediately.
    if let Some(screen) = crate::plugin::pty::manager().read(id) {
        if let Some(text) = screen.pointer("/screen/text").and_then(Value::as_str) {
            if !text.is_empty() {
                send_ws_frame(&mut writer, 2, text.as_bytes()).await?;
            }
        }
    }
    send_ws_frame(&mut writer, 2, b"\x00{\"cursor\":0}").await?;
    let mut output = crate::plugin::pty::manager().subscribe(id).ok_or_else(|| anyhow!("terminal not found"))?;
    loop {
        tokio::select! {
            chunk = output.recv() => {
                match chunk {
                    Ok(bytes) => send_ws_frame(&mut writer, 2, &bytes).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            frame = read_ws_frame(&mut reader) => {
                let Some((opcode, payload)) = frame? else { break };
                match opcode {
                    1 | 2 => {
                        let text = String::from_utf8_lossy(&payload).into_owned();
                        if let Some(control) = text.strip_prefix('\0') {
                            if let Ok(value) = serde_json::from_str::<Value>(control) {
                                if value.get("type").and_then(Value::as_str) == Some("resize") {
                                    let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
                                    let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(40) as u16;
                                    let _ = crate::plugin::pty::manager().resize(id, cols, rows);
                                }
                            }
                        } else {
                            let _ = crate::plugin::pty::manager().write(id, &text);
                        }
                    }
                    8 => break,
                    9 => send_ws_frame(&mut writer, 10, &payload).await?,
                    _ => {}
                }
            }
        }
    }
    let _ = send_ws_frame(&mut writer, 8, &1000u16.to_be_bytes()).await;
    let _ = app;
    Ok(())
}

async fn send_ws_frame(writer: &mut tokio::net::tcp::OwnedWriteHalf, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | opcode);
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_ws_frame(reader: &mut tokio::net::tcp::OwnedReadHalf) -> Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 2];
    if reader.read_exact(&mut head).await.is_err() {
        return Ok(None);
    }
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut length = (head[1] & 0x7f) as u64;
    if length == 126 {
        let mut bytes = [0u8; 2];
        reader.read_exact(&mut bytes).await?;
        length = u16::from_be_bytes(bytes) as u64;
    } else if length == 127 {
        let mut bytes = [0u8; 8];
        reader.read_exact(&mut bytes).await?;
        length = u64::from_be_bytes(bytes);
    }
    if length > 16 * 1024 * 1024 {
        return Err(anyhow!("WebSocket frame too large"));
    }
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload).await?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some((opcode, payload)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_accept_matches_rfc_example() {
        assert_eq!(websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn session_tokens_are_signed_and_scoped() {
        let expires = crate::util::now_ms() / 1000 + 60;
        let token = session_token("secret", expires);
        assert!(verify_session(&token, "secret"));
        assert!(!verify_session(&token, "other"));
        assert!(!verify_session("not-a-token", "secret"));
    }

    #[test]
    fn pairing_codes_are_single_use() {
        let pairing = Mutex::new(HashMap::new());
        let code = issue_pairing(&pairing)["code"].as_str().unwrap().to_string();
        assert!(consume_pairing(&pairing, &code));
        assert!(!consume_pairing(&pairing, &code));
    }

    #[test]
    fn finds_pty_connect_routes() {
        assert_eq!(pty_connect_id("/api/pty/pty_1/connect").as_deref(), Some("pty_1"));
        assert_eq!(pty_connect_id("/api/session/ses_1/pty").as_deref(), Some("ses_1"));
        assert!(pty_connect_id("/api/session").is_none());
    }
}

#[allow(dead_code)]
fn event_type(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::TextDelta { .. } => "session.text.delta",
        AppEvent::ReasoningDelta { .. } => "session.reasoning.delta",
        _ => "message.updated",
    }
}
