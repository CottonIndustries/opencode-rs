//! OpenAI Responses API (`POST /responses`, streaming), used for OpenAI models.

use super::{Event, Request, Sse, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use wreq::ws::message::Message;

#[derive(Default)]
struct StreamState {
    usage: Usage,
    finish: String,
    had_calls: bool,
    summary_parts: usize,
    tool_ids: HashMap<String, String>,
    output_started: bool,
    terminal: bool,
    completed: bool,
    provider_failed: bool,
    response_id: Option<String>,
    output: Vec<Value>,
    error_code: Option<String>,
}

enum WsError {
    Connect(anyhow::Error),
    Stream(anyhow::Error),
}

fn transport(req: &Request) -> Option<String> {
    // Upstream binds the session WebSocket only for primary step requests;
    // title, compaction and generate stay on HTTP.
    if req.hooks.as_ref().is_some_and(|hooks| hooks.kind != "primary") {
        return None;
    }
    req.model
        .options
        .get("transport")
        .and_then(Value::as_str)
        .or_else(|| req.provider.options.extra.get("transport").and_then(Value::as_str))
        .map(String::from)
}

fn data_url(f: &crate::session::FilePart) -> String {
    format!("data:{};base64,{}", f.mime, f.data)
}

fn convert(req: &Request) -> Vec<Value> {
    let mut items = Vec::new();
    for s in req.system.iter().filter(|s| !s.trim().is_empty()) {
        items.push(json!({ "role": "developer", "content": s }));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => content.push(json!({ "type": "input_text", "text": text })),
                        Part::File(f) if f.mime.starts_with("image/") => {
                            content.push(json!({ "type": "input_image", "image_url": data_url(f) }))
                        }
                        Part::File(f) if f.mime == "application/pdf" => {
                            content.push(json!({ "type": "input_file", "filename": f.name, "file_data": data_url(f) }))
                        }
                        Part::Compaction { summary, .. } => content.push(json!({ "type": "input_text", "text": super::compaction_text(summary) })),
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    items.push(json!({ "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let same_provider = m.model.as_deref().is_some_and(|x| x.starts_with(&format!("{}/", req.provider.id)));
                for p in &m.parts {
                    match p {
                        Part::Reasoning { text, redacted: Some(enc), .. } if same_provider => {
                            let summary: Vec<Value> = if text.is_empty() { vec![] } else { vec![json!({ "type": "summary_text", "text": text })] };
                            items.push(json!({ "type": "reasoning", "summary": summary, "encrypted_content": enc }));
                        }
                        Part::Text { text, .. } if !text.is_empty() => {
                            items.push(json!({ "role": "assistant", "content": [{ "type": "output_text", "text": text }] }))
                        }
                        Part::Tool { id, name, input, state, .. } => {
                            let args = if input.get("__invalid_json").is_some() { "{}".to_string() } else { input.to_string() };
                            items.push(json!({ "type": "function_call", "call_id": id, "name": name, "arguments": args }));
                            let (t, _) = super::tool_result_text(state);
                            items.push(json!({ "type": "function_call_output", "call_id": id, "output": t }));
                            if let ToolState::Completed { attachments, .. } = state {
                                let imgs: Vec<Value> = attachments
                                    .iter()
                                    .filter(|a| a.mime.starts_with("image/"))
                                    .map(|a| json!({ "type": "input_image", "image_url": data_url(a) }))
                                    .collect();
                                if !imgs.is_empty() {
                                    let mut content = vec![json!({ "type": "input_text", "text": format!("Attachments from the {name} result:") })];
                                    content.extend(imgs);
                                    items.push(json!({ "role": "user", "content": content }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    items
}

fn build(req: &Request) -> Result<(String, Value, Vec<(String, String)>)> {
    let base = req.provider.base_url(&req.model).ok_or_else(|| anyhow!("no base URL for {}", req.provider.id))?;
    let url = format!("{base}/responses");
    let mut body = json!({
        "model": req.model.id,
        "input": convert(req),
        "stream": true,
        "store": false,
        "prompt_cache_key": req.session.affinity(),
    });
    if req.max_tokens > 0 {
        body["max_output_tokens"] = json!(req.max_tokens);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false }))
                .collect(),
        );
    }
    if req.model.reasoning {
        let mut r = json!({ "summary": "auto" });
        if let Some(v) = req.variant.as_deref().filter(|v| *v != "off") {
            r["effort"] = json!(v);
        }
        body["reasoning"] = r;
        body["include"] = json!(["reasoning.encrypted_content"]);
    } else if req.model.temperature {
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
    }
    if let Value::Object(extra) = &req.model.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }
    if let Value::Object(extra) = &req.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }
    let mut headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    headers.extend(super::client_headers(req));
    if let Some(k) = req.provider.key.as_deref().filter(|k| !k.is_empty()) {
        headers.push(("authorization".into(), format!("Bearer {k}")));
    }
    headers.push(("session_id".into(), req.session.affinity().to_string()));
    for (k, v) in req.provider.options.headers.iter().chain(req.model.headers.iter()) {
        headers.push((k.clone(), v.clone()));
    }
    headers.extend(req.headers.iter().cloned());
    Ok((url, body, headers))
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let (url, body, headers) = build(&req)?;
    if transport(&req).as_deref() == Some("websocket") {
        match stream_websocket(&req, &url, &body, &headers, tx.clone()).await {
            Ok(()) => return Ok(()),
            Err(WsError::Connect(error)) => {
                crate::util::log(&format!("websocket transport unavailable, falling back to HTTP: {error:#}"));
            }
            Err(WsError::Stream(error)) => return Err(error),
        }
    }
    stream_http(req, url, body, headers, tx).await
}

async fn stream_http(req: Request, url: String, body: Value, headers: Vec<(String, String)>, tx: Tx) -> Result<()> {
    let resp = super::post(&tx, &url, &headers, &body, req.hooks.as_ref()).await?;
    let mut sse = Sse::new(resp);
    let mut state = StreamState::default();
    while let Some((event, data)) = sse.next().await? {
        if process_frame(&data, Some(&event), &tx, &mut state)? {
            break;
        }
    }
    finish_stream(&tx, &mut state);
    Ok(())
}

fn websocket_url(http_url: &str) -> Result<String> {
    let mut url = url::Url::parse(http_url)?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        "ws" | "wss" => return Ok(url.to_string()),
        other => return Err(anyhow!("unsupported WebSocket URL scheme: {other}")),
    };
    url.set_scheme(scheme).map_err(|_| anyhow!("invalid WebSocket URL: {http_url}"))?;
    Ok(url.to_string())
}

async fn ws_hook_event(req: &Request, name: &str, mut event: Value) -> Value {
    let Some(hooks) = req.hooks.as_ref() else { return event };
    event["sessionID"] = json!(hooks.session_id);
    event["agent"] = json!(hooks.agent);
    event["model"] = hooks.model.clone();
    event["kind"] = json!(hooks.kind);
    hooks.app.plugins.trigger("session", name, event).await
}

async fn ws_handshake(req: &Request, url: String, headers: &[(String, String)]) -> (String, Vec<(String, String)>) {
    let mut headers = headers.to_vec();
    if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("openai-beta")) {
        headers.push(("openai-beta".into(), "responses_websockets=2026-02-06".into()));
    }
    if req.hooks.is_none() {
        return (url, headers);
    }
    let map: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let event = ws_hook_event(
        req,
        "experimental.ws.handshake",
        json!({ "url": url, "headers": map }),
    )
    .await;
    let next_url = event.get("url").and_then(Value::as_str).map(String::from).unwrap_or(url);
    let next_headers = event
        .get("headers")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                .collect()
        })
        .unwrap_or(headers);
    (next_url, next_headers)
}

async fn ws_frame(req: &Request, name: &str, frame: String) -> String {
    if req.hooks.is_none() {
        return frame;
    }
    let event = ws_hook_event(req, name, json!({ "frame": frame })).await;
    event.get("frame").and_then(Value::as_str).map(String::from).unwrap_or(frame)
}

struct WsFailure {
    connect: bool,
    output_started: bool,
    code: Option<String>,
    error: anyhow::Error,
}

fn too_large(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_lowercase();
    message.contains("1009") || message.contains("message too large") || message.contains("max_message_size")
}

async fn stream_websocket(
    req: &Request,
    http_url: &str,
    body: &Value,
    headers: &[(String, String)],
    tx: Tx,
) -> Result<(), WsError> {
    let url = websocket_url(http_url).map_err(WsError::Connect)?;
    let (url, headers) = ws_handshake(req, url, headers).await;
    let mut lease = crate::provider::ws_transport::Lease::acquire(&req.session.id).await;
    if lease.http_fallback() {
        return Err(WsError::Connect(anyhow!("session transport is pinned to HTTP")));
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        match stream_once(req, &url, body, &headers, &tx, &mut lease).await {
            Ok(()) => return Ok(()),
            Err(failure) if failure.connect => return Err(WsError::Connect(failure.error)),
            Err(failure)
                if !failure.output_started
                    && attempt == 1
                    && matches!(
                        failure.code.as_deref(),
                        Some("previous_response_not_found") | Some("websocket_connection_limit_reached")
                    ) =>
            {
                lease.clear_checkpoint();
                lease.close_channel().await;
            }
            Err(failure) if failure.output_started || failure.code.is_some() => {
                return Err(WsError::Stream(failure.error));
            }
            Err(failure) => return Err(WsError::Stream(failure.error)),
        }
    }
}

fn comparable(value: &Value) -> Value {
    let Some(map) = value.as_object() else { return value.clone() };
    if map.get("role").and_then(Value::as_str) == Some("assistant") {
        let content = map
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        if item.get("type").and_then(Value::as_str) == Some("output_text") {
                            json!({ "type": "output_text", "text": item.get("text").cloned().unwrap_or(Value::Null) })
                        } else {
                            item.clone()
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return json!({ "role": "assistant", "content": content });
    }
    if map.get("type").and_then(Value::as_str) == Some("function_call") {
        let arguments = map
            .get("arguments")
            .map(|arguments| match arguments {
                Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or_else(|_| arguments.clone()),
                _ => arguments.clone(),
            })
            .unwrap_or(Value::Null);
        return json!({
            "type": "function_call",
            "call_id": map.get("call_id").cloned().unwrap_or(Value::Null),
            "name": map.get("name").cloned().unwrap_or(Value::Null),
            "arguments": arguments,
        });
    }
    if map.get("type").and_then(Value::as_str) == Some("reasoning") {
        return json!({
            "type": "reasoning",
            "summary": map.get("summary").cloned().unwrap_or(Value::Null),
            "encrypted_content": map.get("encrypted_content").cloned().unwrap_or(Value::Null),
        });
    }
    value.clone()
}

fn continuation_body(body: &Value, checkpoint: Option<&Value>) -> (Value, bool) {
    let Some(checkpoint) = checkpoint else { return (body.clone(), false) };
    let Some(request) = checkpoint.get("request") else { return (body.clone(), false) };
    let Some(previous_input) = request.get("input").and_then(Value::as_array) else {
        return (body.clone(), false);
    };
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return (body.clone(), false);
    };
    if input.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger")) {
        return (body.clone(), false);
    }
    let mut left = body.clone();
    let mut right = request.clone();
    for value in [&mut left, &mut right] {
        if let Some(map) = value.as_object_mut() {
            map.remove("input");
            map.remove("previous_response_id");
            map.remove("type");
        }
    }
    if left != right {
        return (body.clone(), false);
    }
    let baseline: Vec<Value> = previous_input
        .iter()
        .chain(checkpoint.get("output").and_then(Value::as_array).into_iter().flatten())
        .map(comparable)
        .collect();
    let current: Vec<Value> = input.iter().map(comparable).collect();
    if current.len() <= baseline.len() || !baseline.iter().enumerate().all(|(index, item)| current.get(index) == Some(item)) {
        return (body.clone(), false);
    }
    let Some(response_id) = checkpoint.get("responseID").and_then(Value::as_str) else {
        return (body.clone(), false);
    };
    let mut next = body.clone();
    next["input"] = Value::Array(input[baseline.len()..].to_vec());
    next["previous_response_id"] = json!(response_id);
    (next, true)
}

async fn stream_once(
    req: &Request,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
    tx: &Tx,
    lease: &mut crate::provider::ws_transport::Lease,
) -> Result<(), WsFailure> {
    if let Err(error) = lease.connect(url, headers).await {
        return Err(WsFailure { connect: true, output_started: false, code: None, error });
    }
    let (frame_body, incremental) = continuation_body(body, lease.checkpoint().as_ref());
    if !incremental {
        lease.clear_checkpoint();
    }
    let frame = ws_frame(req, "experimental.ws.send", frame_body.to_string()).await;
    if let Err(error) = lease.send(frame).await {
        lease.poison().await;
        return Err(WsFailure { connect: true, output_started: false, code: None, error });
    }
    let mut state = StreamState::default();
    loop {
        let message = match lease.recv().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                lease.poison().await;
                return Err(WsFailure {
                    connect: false,
                    output_started: state.output_started,
                    code: state.error_code.clone(),
                    error: anyhow!("WebSocket closed before a terminal event"),
                });
            }
            Err(error) => {
                if too_large(&error) {
                    lease.close_channel().await;
                    let connect = !state.output_started;
                    if connect {
                        lease.pin_http();
                    }
                    return Err(WsFailure { connect, output_started: state.output_started, code: None, error });
                }
                lease.poison().await;
                return Err(WsFailure {
                    connect: false,
                    output_started: state.output_started,
                    code: state.error_code.clone(),
                    error,
                });
            }
        };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(frame) => {
                let error = anyhow!("WebSocket closed before a terminal event");
                if frame.is_some_and(|frame| u16::from(frame.code) == 1009) {
                    lease.close_channel().await;
                    let connect = !state.output_started;
                    if connect {
                        lease.pin_http();
                    }
                    let error = anyhow!("WebSocket message too large");
                    return Err(WsFailure { connect, output_started: state.output_started, code: None, error });
                }
                lease.poison().await;
                return Err(WsFailure {
                    connect: false,
                    output_started: state.output_started,
                    code: state.error_code.clone(),
                    error,
                });
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        let text = ws_frame(req, "experimental.ws.receive", text).await;
        match process_frame(&text, None, tx, &mut state) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                let code = state.error_code.clone();
                let output_started = state.output_started;
                if state.provider_failed {
                    lease.close_channel().await;
                } else {
                    lease.poison().await;
                }
                return Err(WsFailure { connect: false, output_started, code, error });
            }
        }
    }
    if !state.terminal {
        lease.poison().await;
        return Err(WsFailure {
            connect: false,
            output_started: state.output_started,
            code: None,
            error: anyhow!("WebSocket exchange ended without a terminal event"),
        });
    }
    finish_stream(tx, &mut state);
    if state.completed {
        if let Some(response_id) = state.response_id.clone() {
            lease.set_checkpoint(json!({
                "responseID": response_id,
                "request": body,
                "output": state.output,
            }));
        } else {
            lease.clear_checkpoint();
        }
    } else {
        lease.clear_checkpoint();
    }
    lease.finish(true);
    Ok(())
}

fn process_frame(data: &str, event: Option<&str>, tx: &Tx, state: &mut StreamState) -> Result<bool> {
    if data.trim() == "[DONE]" {
        return Ok(true);
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else { return Ok(false) };
    let ty = v
        .get("type")
        .and_then(|t| t.as_str())
        .or(event)
        .unwrap_or("")
        .to_string();
    match ty.as_str() {
        "response.output_text.delta" => {
            if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                state.output_started = true;
                let _ = tx.send(Event::TextDelta(d.into()));
            }
        }
        "response.reasoning_summary_part.added" => {
            if state.summary_parts > 0 {
                let _ = tx.send(Event::ReasoningDelta("\n\n".into()));
            }
            state.summary_parts += 1;
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                state.output_started = true;
                let _ = tx.send(Event::ReasoningDelta(d.into()));
            }
        }
        "response.output_item.added" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                state.output_started = true;
                let call_id = item.get("call_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if let Some(item_id) = item.get("id").and_then(|x| x.as_str()) {
                    state.tool_ids.insert(item_id.to_string(), call_id.clone());
                }
                let _ = tx.send(Event::ToolInputStart {
                    id: call_id,
                    name: item.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                });
            }
            if item.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                state.summary_parts = 0;
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = v.get("delta").and_then(|x| x.as_str()).filter(|d| !d.is_empty()) {
                state.output_started = true;
                let item_id = v.get("item_id").and_then(|x| x.as_str()).unwrap_or("");
                let id = state.tool_ids.get(item_id).cloned().unwrap_or_else(|| item_id.to_string());
                let _ = tx.send(Event::ToolInputDelta { id, delta: delta.to_string() });
            }
        }
        "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            state.output.push(item.clone());
            match item.get("type").and_then(|t| t.as_str()) {
                Some("function_call") => {
                    state.had_calls = true;
                    let args = item.get("arguments").and_then(|x| x.as_str()).unwrap_or("");
                    let _ = tx.send(Event::ToolCall {
                        id: item.get("call_id").and_then(|x| x.as_str()).unwrap_or("").into(),
                        name: item.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                        input: super::parse_tool_input(args),
                        meta: None,
                    });
                }
                Some("reasoning") => {
                    let _ = tx.send(Event::ReasoningEnd {
                        signature: None,
                        redacted: item.get("encrypted_content").and_then(|x| x.as_str()).map(String::from),
                        item_id: item.get("id").and_then(|x| x.as_str()).map(String::from),
                    });
                }
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" => {
            let r = v.get("response").cloned().unwrap_or(Value::Null);
            state.terminal = true;
            state.completed = ty == "response.completed";
            state.response_id = r.get("id").and_then(Value::as_str).map(String::from);
            if let Some(output) = r.get("output").and_then(Value::as_array) {
                state.output = output.clone();
            }
            if let Some(u) = r.get("usage") {
                let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                let cached = u.pointer("/input_tokens_details/cached_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                state.usage.input = input.saturating_sub(cached);
                state.usage.cache_read = cached;
                state.usage.output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                state.usage.reasoning = u.pointer("/output_tokens_details/reasoning_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                state.usage.cost = crate::session::reported_cost(u);
            }
            state.finish = if ty == "response.incomplete" {
                match r.pointer("/incomplete_details/reason").and_then(|x| x.as_str()) {
                    Some("content_filter") => "refusal".into(),
                    _ => "length".into(),
                }
            } else if state.had_calls {
                "tool-calls".into()
            } else {
                "stop".into()
            };
            return Ok(true);
        }
        "response.failed" => {
            state.provider_failed = true;
            state.error_code = v
                .pointer("/response/error/code")
                .and_then(Value::as_str)
                .or_else(|| v.pointer("/response/error/type").and_then(Value::as_str))
                .map(String::from);
            let msg = v.pointer("/response/error/message").and_then(|x| x.as_str()).unwrap_or("response failed");
            return Err(anyhow!("{msg}"));
        }
        "error" => {
            state.provider_failed = true;
            state.error_code = v
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| v.pointer("/error/code").and_then(Value::as_str))
                .or_else(|| v.get("type").and_then(Value::as_str))
                .map(String::from);
            let msg = v.get("message").or_else(|| v.pointer("/error/message")).and_then(|x| x.as_str()).unwrap_or("unknown error");
            return Err(anyhow!("{msg}"));
        }
        _ => {}
    }
    Ok(false)
}

fn finish_stream(tx: &Tx, state: &mut StreamState) {
    if state.finish.is_empty() {
        state.finish = "unknown".into();
    }
    let _ = tx.send(Event::Usage(state.usage.clone()));
    let _ = tx.send(Event::Finish(state.finish.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls_follow_the_http_scheme() {
        assert_eq!(websocket_url("https://api.openai.com/v1/responses").unwrap(), "wss://api.openai.com/v1/responses");
        assert_eq!(websocket_url("http://127.0.0.1:8080/responses").unwrap(), "ws://127.0.0.1:8080/responses");
        assert!(websocket_url("ftp://example.com").is_err());
    }

    #[test]
    fn continuation_sends_only_the_delta() {
        let body = json!({
            "model": "gpt",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] },
                { "role": "user", "content": [{ "type": "input_text", "text": "again" }] }
            ],
            "stream": true,
            "store": false
        });
        let checkpoint = json!({
            "responseID": "resp_1",
            "request": {
                "model": "gpt",
                "input": [
                    { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }
                ],
                "stream": true,
                "store": false
            },
            "output": [
                { "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] }
            ]
        });
        let (next, incremental) = continuation_body(&body, Some(&checkpoint));
        assert!(incremental);
        assert_eq!(next["previous_response_id"], "resp_1");
        assert_eq!(next["input"].as_array().unwrap().len(), 1);
        assert_eq!(next["input"][0]["content"][0]["text"], "again");
    }

    #[test]
    fn continuation_matches_function_call_output() {
        let body = json!({
            "model": "gpt",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "read" }] },
                { "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "ok" }
            ],
            "stream": true
        });
        let checkpoint = json!({
            "responseID": "resp_1",
            "request": {
                "model": "gpt",
                "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "read" }] }],
                "stream": true
            },
            "output": [
                { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}" }
            ]
        });
        let (next, incremental) = continuation_body(&body, Some(&checkpoint));
        assert!(incremental);
        assert_eq!(next["input"].as_array().unwrap().len(), 1);
        assert_eq!(next["input"][0]["type"], "function_call_output");
    }

    #[test]
    fn continuation_falls_back_to_full_when_the_prefix_differs() {
        let body = json!({ "model": "gpt", "input": [{ "text": "different" }], "stream": true });
        let checkpoint = json!({
            "responseID": "resp_1",
            "request": { "model": "gpt", "input": [{ "text": "hi" }], "stream": true },
            "output": []
        });
        let (next, incremental) = continuation_body(&body, Some(&checkpoint));
        assert!(!incremental);
        assert_eq!(next, body);
    }

    #[test]
    fn websocket_events_reuse_the_sse_decoder() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = StreamState::default();
        assert!(!process_frame(
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            None,
            &tx,
            &mut state
        )
        .unwrap());
        assert!(matches!(rx.try_recv(), Ok(Event::TextDelta(text)) if text == "hi"));
        assert!(process_frame(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            None,
            &tx,
            &mut state
        )
        .unwrap());
        assert_eq!(state.finish, "stop");
        assert_eq!(state.usage.input, 5);
        assert_eq!(state.usage.output, 2);
    }
}
