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
}

enum WsError {
    Connect(anyhow::Error),
    Stream(anyhow::Error),
}

fn transport(req: &Request) -> Option<String> {
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

async fn stream_websocket(
    req: &Request,
    http_url: &str,
    body: &Value,
    headers: &[(String, String)],
    tx: Tx,
) -> Result<(), WsError> {
    let url = websocket_url(http_url).map_err(WsError::Connect)?;
    let (url, headers) = ws_handshake(req, url, headers).await;
    let mut builder = crate::http::client().websocket(&url);
    for (key, value) in &headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = builder
        .send()
        .await
        .map_err(|error| WsError::Connect(anyhow!("websocket handshake failed: {error}")))?;
    let mut socket = response
        .into_websocket()
        .await
        .map_err(|error| WsError::Connect(anyhow!("websocket upgrade failed: {error}")))?;
    let frame = ws_frame(req, "experimental.ws.send", body.to_string()).await;
    socket
        .send(Message::text(frame))
        .await
        .map_err(|error| WsError::Stream(anyhow!("websocket send failed: {error}")))?;
    let mut state = StreamState::default();
    while let Some(message) = socket.recv().await {
        let message = message.map_err(|error| WsError::Stream(anyhow!("websocket receive failed: {error}")))?;
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        if text.trim().is_empty() {
            continue;
        }
        let text = ws_frame(req, "experimental.ws.receive", text).await;
        match process_frame(&text, None, &tx, &mut state) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                let _ = socket.close(1000, "").await;
                return Err(WsError::Stream(error));
            }
        }
    }
    finish_stream(&tx, &mut state);
    let _ = socket.close(1000, "").await;
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
                let _ = tx.send(Event::ReasoningDelta(d.into()));
            }
        }
        "response.output_item.added" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
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
                let item_id = v.get("item_id").and_then(|x| x.as_str()).unwrap_or("");
                let id = state.tool_ids.get(item_id).cloned().unwrap_or_else(|| item_id.to_string());
                let _ = tx.send(Event::ToolInputDelta { id, delta: delta.to_string() });
            }
        }
        "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
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
            let msg = v.pointer("/response/error/message").and_then(|x| x.as_str()).unwrap_or("response failed");
            return Err(anyhow!("{msg}"));
        }
        "error" => {
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
