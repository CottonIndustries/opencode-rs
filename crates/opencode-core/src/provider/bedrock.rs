//! Amazon Bedrock Converse API (`/model/{id}/converse-stream`, binary event stream).

use super::{Event, Request, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn image_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    })
}

fn file_block(f: &crate::session::FilePart) -> Option<Value> {
    if let Some(format) = image_format(&f.mime) {
        return Some(json!({
            "image": { "format": format, "source": { "bytes": f.data } }
        }));
    }
    if f.mime == "application/pdf" {
        return Some(json!({
            "document": { "format": "pdf", "name": "document", "source": { "bytes": f.data } }
        }));
    }
    None
}

fn convert(req: &Request) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => {
                            content.push(json!({ "text": text }))
                        }
                        Part::File(f) => {
                            if let Some(block) = file_block(f) {
                                content.push(block);
                            }
                        }
                        Part::Compaction { summary, .. } => {
                            content.push(json!({ "text": super::compaction_text(summary) }))
                        }
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
                            content.push(json!({ "text": text }))
                        }
                        Part::Reasoning { text, .. } if !text.trim().is_empty() => content.push(
                            json!({ "reasoningContent": { "reasoningText": { "text": text } } }),
                        ),
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
                            let id = sanitize_id(id);
                            content.push(json!({
                                "toolUse": { "toolUseId": id, "name": name, "input": input }
                            }));
                            let (text, is_error) = super::tool_result_text(state);
                            let mut blocks = vec![json!({ "text": text })];
                            if let ToolState::Completed { attachments, .. } = state {
                                for a in attachments {
                                    if let Some(block) = file_block(a) {
                                        blocks.push(block);
                                    }
                                }
                            }
                            results.push(json!({
                                "toolResult": {
                                    "toolUseId": id,
                                    "content": blocks,
                                    "status": if is_error { "error" } else { "success" },
                                }
                            }));
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

fn region(req: &Request) -> String {
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .ok()
        .or_else(|| {
            req.provider
                .options
                .extra
                .get("region")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_else(|| "us-east-1".into())
}

fn endpoint(req: &Request) -> String {
    if let Some(base) = req
        .provider
        .options
        .base_url
        .clone()
        .or_else(|| req.model.api.clone())
    {
        return base.trim_end_matches('/').to_string();
    }
    format!("https://bedrock-runtime.{}.amazonaws.com", region(req))
}

fn credentials(req: &Request) -> Option<(String, String, Option<String>)> {
    let extra = &req.provider.options.extra;
    let access = std::env::var("AWS_ACCESS_KEY_ID")
        .ok()
        .or_else(|| {
            extra
                .get("accessKeyId")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            crate::auth::get(&req.provider.id).and_then(|credential| match credential {
                crate::auth::Credential::Api {
                    metadata: Some(metadata),
                    ..
                } => metadata.get("accessKeyId").cloned(),
                _ => None,
            })
        })?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
        .ok()
        .or_else(|| {
            extra
                .get("secretAccessKey")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            crate::auth::get(&req.provider.id).and_then(|credential| match credential {
                crate::auth::Credential::Api {
                    metadata: Some(metadata),
                    ..
                } => metadata.get("secretAccessKey").cloned(),
                _ => None,
            })
        })?;
    let token = std::env::var("AWS_SESSION_TOKEN")
        .ok()
        .or_else(|| {
            extra
                .get("sessionToken")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            crate::auth::get(&req.provider.id).and_then(|credential| match credential {
                crate::auth::Credential::Api {
                    metadata: Some(metadata),
                    ..
                } => metadata.get("sessionToken").cloned(),
                _ => None,
            })
        });
    Some((access, secret, token))
}

fn sign(
    access: &str,
    secret: &str,
    token: Option<&str>,
    region: &str,
    host: &str,
    path: &str,
    body: &[u8],
) -> Vec<(String, String)> {
    let now = chrono::Utc::now();
    let date = now.format("%Y%m%d").to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let service = "bedrock";
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("host".to_string(), host.to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    if let Some(token) = token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let payload_hash = sha256_hex(body);
    let canonical_request =
        format!("POST\n{path}\n\n{canonical_headers}\n{signed}\n{payload_hash}");
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={signature}"
    );
    let mut out = headers;
    out.push(("authorization".to_string(), authorization));
    out.push((
        "accept".to_string(),
        "application/vnd.amazon.eventstream".to_string(),
    ));
    out
}

fn next_frame(buf: &mut Vec<u8>) -> Option<(Vec<(String, String)>, Vec<u8>)> {
    if buf.len() < 12 {
        return None;
    }
    let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if total < 16 || buf.len() < total {
        return None;
    }
    let headers_end = 12 + headers_len;
    if headers_end + 4 > total {
        buf.drain(..total);
        return None;
    }
    let mut pos = 12;
    let mut headers = Vec::new();
    while pos < headers_end {
        let name_len = buf[pos] as usize;
        pos += 1;
        if pos + name_len > headers_end {
            break;
        }
        let name = String::from_utf8_lossy(&buf[pos..pos + name_len]).to_string();
        pos += name_len;
        if pos >= headers_end {
            break;
        }
        let ty = buf[pos];
        pos += 1;
        if pos + 2 > headers_end {
            break;
        }
        let value_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
        if pos + value_len > headers_end {
            break;
        }
        let raw = &buf[pos..pos + value_len];
        pos += value_len;
        let value = if ty == 7 {
            String::from_utf8_lossy(raw).to_string()
        } else {
            hex(raw)
        };
        headers.push((name, value));
    }
    let payload = buf[headers_end..total - 4].to_vec();
    buf.drain(..total);
    Some((headers, payload))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn finish_reason(reason: &str) -> String {
    match reason {
        "tool_use" => "tool-calls",
        "max_tokens" => "length",
        "content_filtered" | "guardrail_intervened" => "content-filter",
        _ => "stop",
    }
    .to_string()
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let base = endpoint(&req);
    let url = format!(
        "{base}/model/{}/converse-stream",
        percent_encode(&req.model.id)
    );
    let parsed = url::Url::parse(&url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("bedrock endpoint has no host"))?
        .to_string();
    let path = parsed.path().to_string();

    let mut body = json!({ "messages": convert(&req) });
    let system: Vec<Value> = req
        .system
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| json!({ "text": s }))
        .collect();
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let max_tokens = if req.max_tokens > 0 {
        req.max_tokens
    } else {
        super::max_output(&req.model)
    };
    body["inferenceConfig"] = json!({ "maxTokens": max_tokens });
    if let Some(temperature) = req.temperature {
        body["inferenceConfig"]["temperature"] = json!(temperature);
    }
    if let Some(top_p) = req.top_p {
        body["inferenceConfig"]["topP"] = json!(top_p);
    }
    if !req.tools.is_empty() {
        body["toolConfig"] = json!({
            "tools": req.tools.iter().map(|tool| json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": tool.schema },
                }
            })).collect::<Vec<_>>()
        });
    }
    if let Some(options) = req.options.as_object() {
        if !options.is_empty() {
            body["additionalModelRequestFields"] = Value::Object(options.clone());
        }
    }

    let body_bytes = serde_json::to_vec(&body)?;
    let headers = if let Some(bearer) = std::env::var("AWS_BEARER_TOKEN_BEDROCK")
        .ok()
        .or_else(|| req.provider.key.clone())
    {
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {bearer}")),
            (
                "accept".to_string(),
                "application/vnd.amazon.eventstream".to_string(),
            ),
        ]
    } else {
        let (access, secret, token) = credentials(&req).ok_or_else(|| {
            anyhow!("provider {} needs AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (or AWS_BEARER_TOKEN_BEDROCK)", req.provider.id)
        })?;
        sign(
            &access,
            &secret,
            token.as_deref(),
            &region(&req),
            &host,
            &path,
            &body_bytes,
        )
    };

    let resp = super::post(&tx, &url, &headers, &body, req.hooks.as_ref()).await?;
    let mut stream = resp.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut tools: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);
        while let Some((headers, payload)) = next_frame(&mut buffer) {
            let message_type = header(&headers, ":message-type").unwrap_or("event");
            let event_type = header(&headers, ":event-type")
                .or_else(|| header(&headers, ":exception-type"))
                .unwrap_or_default();
            let value: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
            if message_type == "exception" || message_type == "error" {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| header(&headers, ":exception-type"))
                    .unwrap_or("bedrock error");
                return Err(anyhow!("{message}"));
            }
            match event_type {
                "messageStart" => {}
                "contentBlockStart" => {
                    if let Some(tool) = value.pointer("/start/toolUse") {
                        let id = tool
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = tool
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = tx.send(Event::ToolInputStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        let index = value
                            .get("contentBlockIndex")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        tools.insert(index, (id, name, String::new()));
                    }
                }
                "contentBlockDelta" => {
                    let index = value
                        .get("contentBlockIndex")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if let Some(delta) = value.get("delta") {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                let _ = tx.send(Event::TextDelta(text.to_string()));
                            }
                        }
                        if let Some(text) = delta
                            .pointer("/reasoningContent/text")
                            .and_then(Value::as_str)
                        {
                            if !text.is_empty() {
                                let _ = tx.send(Event::ReasoningDelta(text.to_string()));
                            }
                        }
                        if let Some(partial) =
                            delta.pointer("/toolUse/input").and_then(Value::as_str)
                        {
                            if let Some((id, _, json)) = tools.get_mut(&index) {
                                json.push_str(partial);
                                let _ = tx.send(Event::ToolInputDelta {
                                    id: id.clone(),
                                    delta: partial.to_string(),
                                });
                            }
                        }
                    }
                }
                "contentBlockStop" => {
                    let index = value
                        .get("contentBlockIndex")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if let Some((id, name, json)) = tools.remove(&index) {
                        let input: Value =
                            serde_json::from_str(&json).unwrap_or_else(|_| json!({}));
                        let _ = tx.send(Event::ToolCall {
                            id,
                            name,
                            input,
                            meta: None,
                        });
                    }
                }
                "messageStop" => {
                    let reason = value
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn");
                    stop = Some(finish_reason(reason));
                }
                "metadata" => {
                    if let Some(u) = value.get("usage") {
                        usage.input = u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
                        usage.output = u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                        usage.cache_read = u
                            .get("cacheReadInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        usage.cache_write = u
                            .get("cacheWriteInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let _ = tx.send(Event::Usage(usage.clone()));
                    }
                }
                _ => {}
            }
        }
    }
    let _ = tx.send(Event::Finish(stop.unwrap_or_else(|| "stop".into())));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_decode_headers_and_payload() {
        let payload = br#"{"stopReason":"end_turn"}"#;
        let mut frame = Vec::new();
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", "messageStop"), (":message-type", "event")] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = 12 + headers.len() + payload.len() + 4;
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0u32.to_be_bytes());
        let mut buffer = frame;
        let (headers, payload) = next_frame(&mut buffer).expect("frame");
        assert!(buffer.is_empty());
        assert_eq!(header(&headers, ":event-type"), Some("messageStop"));
        assert_eq!(
            serde_json::from_slice::<Value>(&payload).unwrap()["stopReason"],
            "end_turn"
        );
    }

    #[test]
    fn finish_reasons_match_bedrock() {
        assert_eq!(finish_reason("tool_use"), "tool-calls");
        assert_eq!(finish_reason("max_tokens"), "length");
        assert_eq!(finish_reason("end_turn"), "stop");
    }
}
