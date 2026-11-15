//! Google Vertex AI. Gemini models use streamGenerateContent; Anthropic models use
//! streamRawPredict. Authentication uses a bearer token from GOOGLE_VERTEX_ACCESS_TOKEN,
//! application default credentials, or gcloud.

use super::{Event, Request, Sse, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

static TOKEN: OnceLock<Mutex<Option<(String, Instant)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(String, Instant)>> {
    TOKEN.get_or_init(|| Mutex::new(None))
}

fn b64(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn adc_path() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }
    let mut candidates = Vec::new();
    if let Some(config) = dirs::config_dir() {
        candidates.push(
            config
                .join("gcloud")
                .join("application_default_credentials.json"),
        );
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(
            std::path::PathBuf::from(appdata)
                .join("gcloud")
                .join("application_default_credentials.json"),
        );
    }
    candidates.into_iter().find(|path| path.is_file())
}

async fn token_request(url: &str, form: &[(&str, &str)]) -> Result<Value> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = crate::http::client()
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("google token exchange failed ({status}): {text}"));
    }
    Ok(serde_json::from_str(&text)?)
}

async fn service_account_token(json: &Value) -> Result<String> {
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use sha2::Sha256;

    let key_pem = json
        .get("private_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("service account has no private_key"))?;
    let client_email = json
        .get("client_email")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("service account has no client_email"))?;
    let token_uri = json
        .get("token_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://oauth2.googleapis.com/token");
    let now = chrono::Utc::now().timestamp();
    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": client_email,
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "aud": token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let signing_input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let private = rsa::RsaPrivateKey::from_pkcs8_pem(key_pem)?;
    let signer = SigningKey::<Sha256>::new(private);
    let signature = signer.sign(signing_input.as_bytes());
    let assertion = format!("{signing_input}.{}", b64(&signature.to_vec()));
    let token = token_request(
        token_uri,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &assertion),
        ],
    )
    .await?;
    token
        .get("access_token")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("service account token response has no access_token"))
}

async fn fetch_token() -> Result<String> {
    if let Some(path) = adc_path() {
        let text = std::fs::read_to_string(&path)?;
        let json: Value = serde_json::from_str(&text)?;
        match json.get("type").and_then(Value::as_str) {
            Some("service_account") => return service_account_token(&json).await,
            Some("authorized_user") => {
                let client_id = json
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let client_secret = json
                    .get("client_secret")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let refresh = json
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!("application default credentials have no refresh_token")
                    })?;
                let token = token_request(
                    "https://oauth2.googleapis.com/token",
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("refresh_token", refresh),
                    ],
                )
                .await?;
                return token
                    .get("access_token")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| anyhow!("google token response has no access_token"));
            }
            _ => {}
        }
    }
    for command in ["gcloud", "gcloud.cmd"] {
        if let Ok(output) = std::process::Command::new(command)
            .args(["auth", "print-access-token"])
            .output()
        {
            if output.status.success() {
                let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !token.is_empty() {
                    return Ok(token);
                }
            }
        }
    }
    Err(anyhow!("google-vertex needs GOOGLE_VERTEX_ACCESS_TOKEN, application default credentials, or gcloud auth login"))
}

async fn access_token(req: &Request) -> Result<String> {
    if let Ok(token) = std::env::var("GOOGLE_VERTEX_ACCESS_TOKEN")
        .or_else(|_| std::env::var("GOOGLE_ACCESS_TOKEN"))
    {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if let Some(key) = req
        .provider
        .key
        .as_deref()
        .filter(|key| key.starts_with("ya29."))
    {
        return Ok(key.to_string());
    }
    let mut guard = cache().lock().await;
    if let Some((token, expiry)) = guard.as_ref() {
        if *expiry > Instant::now() {
            return Ok(token.clone());
        }
    }
    let token = fetch_token().await?;
    *guard = Some((token.clone(), Instant::now() + Duration::from_secs(3000)));
    Ok(token)
}

fn project(req: &Request) -> Result<String> {
    for key in [
        "GOOGLE_VERTEX_PROJECT",
        "GOOGLE_CLOUD_PROJECT",
        "GCLOUD_PROJECT",
    ] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    if let Some(path) = adc_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<Value>(&text) {
                for key in ["quota_project_id", "project_id"] {
                    if let Some(value) = json.get(key).and_then(Value::as_str) {
                        if !value.is_empty() {
                            return Ok(value.to_string());
                        }
                    }
                }
            }
        }
    }
    let _ = req;
    Err(anyhow!(
        "google-vertex needs GOOGLE_VERTEX_PROJECT or GOOGLE_CLOUD_PROJECT"
    ))
}

fn location() -> String {
    std::env::var("GOOGLE_VERTEX_LOCATION")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_REGION"))
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "us-central1".into())
}

fn is_anthropic(req: &Request) -> bool {
    let npm = req
        .model
        .npm
        .as_deref()
        .or(Some(req.provider.npm.as_str()))
        .unwrap_or_default();
    npm.contains("anthropic") || req.model.id.starts_with("claude")
}

fn clean_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                if matches!(
                    k.as_str(),
                    "additionalProperties"
                        | "$schema"
                        | "default"
                        | "examples"
                        | "$ref"
                        | "$defs"
                        | "const"
                ) {
                    continue;
                }
                out.insert(k.clone(), clean_schema(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

fn google_convert(req: &Request) -> Vec<Value> {
    let mut contents: Vec<(String, Vec<Value>)> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => {
                            parts.push(json!({ "text": text }))
                        }
                        Part::File(f)
                            if f.mime.starts_with("image/") || f.mime == "application/pdf" =>
                        {
                            parts.push(
                                json!({ "inlineData": { "mimeType": f.mime, "data": f.data } }),
                            )
                        }
                        Part::Compaction { summary, .. } => {
                            parts.push(json!({ "text": super::compaction_text(summary) }))
                        }
                        _ => {}
                    }
                }
                contents.push(("user".into(), parts));
            }
            Role::Assistant => {
                let mut parts = Vec::new();
                let mut results = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => {
                            parts.push(json!({ "text": text }))
                        }
                        Part::Tool {
                            name,
                            input,
                            state,
                            provider_meta,
                            ..
                        } => {
                            let args = if input.is_object() && input.get("__invalid_json").is_none()
                            {
                                input.clone()
                            } else {
                                json!({})
                            };
                            let mut call =
                                json!({ "functionCall": { "name": name, "args": args } });
                            if let Some(sig) = provider_meta
                                .as_ref()
                                .and_then(|m| m.get("thoughtSignature"))
                            {
                                call["thoughtSignature"] = sig.clone();
                            }
                            parts.push(call);
                            let (t, is_err) = super::tool_result_text(state);
                            let mut response = serde_json::Map::new();
                            response.insert(
                                if is_err { "error" } else { "content" }.into(),
                                Value::String(t),
                            );
                            results.push(json!({ "functionResponse": { "name": name, "response": response } }));
                            if let ToolState::Completed { attachments, .. } = state {
                                for a in attachments {
                                    results.push(json!({ "inlineData": { "mimeType": a.mime, "data": a.data } }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                contents.push(("model".into(), parts));
                if !results.is_empty() {
                    contents.push(("user".into(), results));
                }
            }
        }
    }
    super::merge_consecutive(contents)
        .into_iter()
        .map(|(role, parts)| json!({ "role": role, "parts": parts }))
        .collect()
}

fn google_body(req: &Request) -> Value {
    let mut generation = if req.max_tokens > 0 {
        json!({ "maxOutputTokens": req.max_tokens })
    } else {
        json!({})
    };
    if req.model.temperature {
        if let Some(t) = req.temperature {
            generation["temperature"] = json!(t);
        }
    }
    if req.model.reasoning {
        let mut tc = json!({ "includeThoughts": true });
        match req.variant.as_deref() {
            Some("off") => tc = json!({ "thinkingBudget": 0 }),
            Some(v) if req.model.id.contains("gemini-3") => tc["thinkingLevel"] = json!(v),
            Some("low") => tc["thinkingBudget"] = json!(2048),
            Some("medium") => tc["thinkingBudget"] = json!(8192),
            Some("high") => tc["thinkingBudget"] = json!(24576),
            _ => {}
        }
        generation["thinkingConfig"] = tc;
    }
    let mut body = json!({ "contents": google_convert(req), "generationConfig": generation });
    if let Value::Object(extra) = &req.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }
    let system = req
        .system
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| json!({ "text": s }))
        .collect::<Vec<_>>();
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": system });
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "parameters": clean_schema(&t.schema) }))
            .collect();
        body["tools"] = json!([{ "functionDeclarations": decls }]);
    }
    body
}

fn file_block(f: &crate::session::FilePart) -> Option<Value> {
    if f.mime.starts_with("image/") {
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": f.mime, "data": f.data }
        }));
    }
    if f.mime == "application/pdf" {
        return Some(json!({
            "type": "document",
            "source": { "type": "base64", "media_type": f.mime, "data": f.data }
        }));
    }
    None
}

fn anthropic_convert(req: &Request) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => {
                            content.push(json!({ "type": "text", "text": text }))
                        }
                        Part::File(f) => {
                            if let Some(block) = file_block(f) {
                                content.push(block);
                            }
                        }
                        Part::Compaction { summary, .. } => content.push(
                            json!({ "type": "text", "text": super::compaction_text(summary) }),
                        ),
                        _ => {}
                    }
                }
                out.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                let mut content = Vec::new();
                let mut results = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.trim().is_empty() => {
                            content.push(json!({ "type": "text", "text": text }))
                        }
                        Part::Reasoning {
                            text, signature, ..
                        } if !text.trim().is_empty() => {
                            let mut block = json!({ "type": "thinking", "thinking": text });
                            if let Some(signature) = signature {
                                block["signature"] = json!(signature);
                            }
                            content.push(block);
                        }
                        Part::Tool {
                            id,
                            name,
                            input,
                            state,
                            ..
                        } => {
                            let input =
                                if input.is_object() && input.get("__invalid_json").is_none() {
                                    input.clone()
                                } else {
                                    json!({})
                                };
                            content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
                            let (text, is_error) = super::tool_result_text(state);
                            results.push(json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error }));
                            if let ToolState::Completed { attachments, .. } = state {
                                for a in attachments {
                                    if let Some(block) = file_block(a) {
                                        results.push(block);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
                if !results.is_empty() {
                    out.push(json!({ "role": "user", "content": results }));
                }
            }
        }
    }
    out
}

fn anthropic_body(req: &Request, max_tokens: u64) -> Value {
    let (thinking, effort, _) =
        super::anthropic::reasoning(&req.model, req.variant.as_deref(), max_tokens);
    let mut body = json!({
        "anthropic_version": "vertex-2023-10-16",
        "max_tokens": max_tokens,
        "stream": true,
        "messages": anthropic_convert(req),
    });
    let system: Vec<Value> = req
        .system
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| json!({ "type": "text", "text": s }))
        .collect();
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema })).collect());
    }
    if let Some(t) = thinking {
        body["thinking"] = t;
    }
    if let Some(e) = effort {
        body["output_config"] = json!({ "effort": e });
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = json!(p);
    }
    if let Value::Object(extra) = &req.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }
    body
}

async fn anthropic_stream(req: Request, tx: Tx, url: String, token: String) -> Result<()> {
    let max_tokens = if req.max_tokens > 0 {
        req.max_tokens
    } else {
        super::max_output(&req.model)
    };
    let body = anthropic_body(&req, max_tokens);
    let mut headers: Vec<(String, String)> = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
    ];
    headers.extend(super::client_headers(&req));
    headers.extend(req.headers.iter().cloned());
    let resp = super::post(&tx, &url, &headers, &body, req.hooks.as_ref()).await?;
    let mut sse = Sse::new(resp);

    enum Block {
        Text,
        Thinking {
            sig: String,
        },
        Redacted(String),
        Tool {
            id: String,
            name: String,
            json: String,
        },
        Other,
    }
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop: Option<String> = None;
    while let Some((event, data)) = sse.next().await? {
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let ty = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or(&event)
            .to_string();
        match ty.as_str() {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    usage.input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    usage.cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    usage.cache_write = u
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    usage.output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                let block = match cb.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = cb
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            let _ = tx.send(Event::TextDelta(text.into()));
                        }
                        Block::Text
                    }
                    Some("thinking") => Block::Thinking { sig: String::new() },
                    Some("redacted_thinking") => {
                        Block::Redacted(cb.get("data").and_then(Value::as_str).unwrap_or("").into())
                    }
                    Some("tool_use") => {
                        let id = cb
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = cb
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = tx.send(Event::ToolInputStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        Block::Tool {
                            id,
                            name,
                            json: String::new(),
                        }
                    }
                    _ => Block::Other,
                };
                blocks.insert(idx, block);
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match (d.get("type").and_then(Value::as_str), blocks.get_mut(&idx)) {
                    (Some("text_delta"), _) => {
                        if let Some(text) = d.get("text").and_then(Value::as_str) {
                            let _ = tx.send(Event::TextDelta(text.into()));
                        }
                    }
                    (Some("thinking_delta"), _) => {
                        if let Some(text) = d.get("thinking").and_then(Value::as_str) {
                            let _ = tx.send(Event::ReasoningDelta(text.into()));
                        }
                    }
                    (Some("signature_delta"), Some(Block::Thinking { sig })) => {
                        sig.push_str(d.get("signature").and_then(Value::as_str).unwrap_or(""));
                    }
                    (Some("input_json_delta"), Some(Block::Tool { id, json, .. })) => {
                        if let Some(delta) = d
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .filter(|delta| !delta.is_empty())
                        {
                            json.push_str(delta);
                            let _ = tx.send(Event::ToolInputDelta {
                                id: id.clone(),
                                delta: delta.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                match blocks.remove(&idx) {
                    Some(Block::Thinking { sig }) => {
                        let _ = tx.send(Event::ReasoningEnd {
                            signature: Some(sig).filter(|sig| !sig.is_empty()),
                            redacted: None,
                            item_id: None,
                        });
                    }
                    Some(Block::Redacted(data)) => {
                        let _ = tx.send(Event::ReasoningEnd {
                            signature: None,
                            redacted: Some(data),
                            item_id: None,
                        });
                    }
                    Some(Block::Tool { id, name, json }) => {
                        let _ = tx.send(Event::ToolCall {
                            id,
                            name,
                            input: super::parse_tool_input(&json),
                            meta: None,
                        });
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    stop = Some(reason.to_string());
                }
                if let Some(u) = v.get("usage") {
                    if let Some(output) = u.get("output_tokens").and_then(Value::as_u64) {
                        usage.output = output;
                    }
                    if let Some(input) = u.get("input_tokens").and_then(Value::as_u64) {
                        usage.input = input;
                    }
                    if let Some(cache) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
                        usage.cache_read = cache;
                    }
                    if let Some(cache) =
                        u.get("cache_creation_input_tokens").and_then(Value::as_u64)
                    {
                        usage.cache_write = cache;
                    }
                }
            }
            "message_stop" => break,
            "error" => {
                let message = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(anyhow!("{message}"));
            }
            _ => {}
        }
    }
    let _ = tx.send(Event::Usage(usage));
    let finish = match stop.as_deref() {
        Some("tool_use") => "tool-calls",
        Some("max_tokens") => "length",
        Some("refusal") => "refusal",
        _ => "stop",
    };
    let _ = tx.send(Event::Finish(finish.into()));
    Ok(())
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let token = access_token(&req).await?;
    let base = req
        .provider
        .options
        .base_url
        .clone()
        .or_else(|| req.model.api.clone());
    let anthropic = is_anthropic(&req);
    let url = if let Some(base) = base {
        let action = if anthropic {
            "streamRawPredict"
        } else {
            "streamGenerateContent?alt=sse"
        };
        let publisher = if anthropic { "anthropic" } else { "google" };
        format!(
            "{}/publishers/{publisher}/models/{}:{action}",
            base.trim_end_matches('/'),
            req.model.id
        )
    } else {
        let project = project(&req)?;
        let location = location();
        let host = if location == "global" {
            "aiplatform.googleapis.com".to_string()
        } else {
            format!("{location}-aiplatform.googleapis.com")
        };
        let action = if anthropic {
            "streamRawPredict"
        } else {
            "streamGenerateContent?alt=sse"
        };
        let publisher = if anthropic { "anthropic" } else { "google" };
        format!("https://{host}/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{}:{action}", req.model.id)
    };
    if anthropic {
        return anthropic_stream(req, tx, url, token).await;
    }

    let body = google_body(&req);
    let mut headers: Vec<(String, String)> = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
    ];
    headers.extend(super::client_headers(&req));
    for (k, v) in req
        .provider
        .options
        .headers
        .iter()
        .chain(req.model.headers.iter())
    {
        headers.push((k.clone(), v.clone()));
    }
    headers.extend(req.headers.iter().cloned());

    let resp = super::post(&tx, &url, &headers, &body, req.hooks.as_ref()).await?;
    let mut sse = Sse::new(resp);
    let mut usage = Usage::default();
    let mut finish = "unknown".to_string();
    let mut had_calls = false;
    let mut in_thought = false;
    while let Some((_, data)) = sse.next().await? {
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some(err) = v.get("error") {
            return Err(anyhow!(
                "{}",
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("gemini error")
            ));
        }
        if let Some(u) = v.get("usageMetadata") {
            let prompt = u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = u
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            usage.input = prompt.saturating_sub(cached);
            usage.cache_read = cached;
            usage.reasoning = u
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            usage.output = u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + usage.reasoning;
        }
        let Some(cand) = v.pointer("/candidates/0") else {
            continue;
        };
        for p in cand
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(fc) = p.get("functionCall") {
                if in_thought {
                    in_thought = false;
                    let _ = tx.send(Event::ReasoningEnd {
                        signature: None,
                        redacted: None,
                        item_id: None,
                    });
                }
                had_calls = true;
                let name = fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let meta = p
                    .get("thoughtSignature")
                    .map(|s| json!({ "thoughtSignature": s }));
                let _ = tx.send(Event::ToolCall {
                    id: fc
                        .get("id")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| crate::util::id("call")),
                    name,
                    input: fc.get("args").cloned().unwrap_or(json!({})),
                    meta,
                });
                continue;
            }
            if let Some(text) = p.get("text").and_then(Value::as_str) {
                if p.get("thought").and_then(Value::as_bool) == Some(true) {
                    in_thought = true;
                    let _ = tx.send(Event::ReasoningDelta(text.to_string()));
                } else {
                    if in_thought {
                        in_thought = false;
                        let _ = tx.send(Event::ReasoningEnd {
                            signature: None,
                            redacted: None,
                            item_id: None,
                        });
                    }
                    let _ = tx.send(Event::TextDelta(text.to_string()));
                }
            }
        }
        if let Some(reason) = cand.get("finishReason").and_then(Value::as_str) {
            finish = match reason {
                "STOP" => "stop",
                "MAX_TOKENS" => "length",
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content-filter",
                other => other,
            }
            .to_string();
        }
    }
    if in_thought {
        let _ = tx.send(Event::ReasoningEnd {
            signature: None,
            redacted: None,
            item_id: None,
        });
    }
    if had_calls {
        finish = "tool-calls".into();
    }
    let _ = tx.send(Event::Usage(usage));
    let _ = tx.send(Event::Finish(finish));
    Ok(())
}
