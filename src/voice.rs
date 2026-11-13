//! Server-brokered realtime voice sessions.

use crate::auth::Credential;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

const OPENAI_BASE: &str = "https://api.openai.com/v1";
const XAI_BASE: &str = "https://api.x.ai/v1";
const GOOGLE_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const LIVE_BASE: &str = "https://opencode.ai/inference/openai/v1";

struct Engine {
    id: &'static str,
    family: &'static str,
    model: &'static str,
    provider: &'static str,
    transports: &'static [&'static str],
}

const ENGINES: &[Engine] = &[
    Engine { id: "openai-live", family: "gpt-live", model: "gpt-live-1", provider: "openai", transports: &["webrtc"] },
    Engine { id: "openai-realtime", family: "gpt-realtime", model: "gpt-realtime-2.1", provider: "openai", transports: &["webrtc", "websocket"] },
    Engine { id: "xai-realtime", family: "grok-voice", model: "grok-voice-3.0", provider: "xai", transports: &["websocket"] },
    Engine { id: "gemini-live", family: "gemini-3.8-live", model: "gemini-3.8-live", provider: "google", transports: &["websocket"] },
];

fn engine(model: &str) -> Option<&'static Engine> {
    ENGINES.iter().find(|engine| model == engine.family || model.starts_with(&format!("{}-", engine.family)))
}

struct Access {
    token: String,
    base: Option<String>,
    org: Option<String>,
    provider: String,
}

fn metadata_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(key).and_then(Value::as_str).map(String::from))
}

fn credential(provider: &str) -> Option<Access> {
    let all = crate::auth::all();
    if let Some(value) = all.get(provider) {
        let (token, org) = match value {
            Credential::Api { key, metadata } => {
                let org = metadata.as_ref().and_then(|data| ["orgID", "orgId", "org_id"].iter().find_map(|key| data.get(*key).cloned()));
                (key.clone(), org)
            }
            Credential::Oauth { access, metadata, .. } => {
                let org = metadata.as_ref().and_then(|data| metadata_string(data, &["orgID", "orgId", "org_id"]));
                (access.clone(), org)
            }
            Credential::Wellknown { token, .. } => (token.clone(), None),
        };
        let base = if provider == "opencode" { Some(LIVE_BASE.to_string()) } else { None };
        return Some(Access { token, base, org, provider: provider.to_string() });
    }
    let env = match provider {
        "openai" => std::env::var("OPENAI_API_KEY").ok(),
        "xai" => std::env::var("XAI_API_KEY").ok(),
        "google" => std::env::var("GEMINI_API_KEY").ok().or_else(|| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY").ok()).or_else(|| std::env::var("GOOGLE_API_KEY").ok()),
        _ => None,
    };
    env.filter(|key| !key.is_empty()).map(|token| Access { token, base: None, org: None, provider: provider.to_string() })
}

fn source(engine: &Engine) -> Option<Access> {
    if let Some(access) = credential(engine.provider) {
        return Some(access);
    }
    if engine.id != "openai-live" {
        return None;
    }
    credential("opencode")
}

pub fn models() -> Value {
    let list = ENGINES
        .iter()
        .map(|engine| {
            let found = source(engine);
            json!({
                "model": engine.model,
                "provider": found.as_ref().map(|access| access.provider.clone()).unwrap_or_else(|| engine.provider.to_string()),
                "transports": engine.transports,
                "available": found.is_some(),
            })
        })
        .collect::<Vec<_>>();
    let default = ENGINES
        .iter()
        .find(|engine| source(engine).is_some())
        .map(|engine| engine.model);
    match default {
        Some(model) => json!({ "models": list, "default": model }),
        None => json!({ "models": list }),
    }
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

async fn post_json(url: &str, access: &Access, body: Value) -> Result<(u16, Value)> {
    let mut builder = crate::http::client()
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", access.token));
    if let Some(org) = &access.org {
        builder = builder.header("x-opencode-org-id", org);
    }
    let response = crate::http::send(builder.body(json_text(&body)).timeout(std::time::Duration::from_secs(30))).await?;
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, value))
}

async fn client_secret(base: &str, model: &str, access: &Access, body: Value) -> Result<Value> {
    let (status, value) = post_json(&format!("{base}/realtime/client_secrets"), access, body).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("realtime client secret failed: HTTP {status} {value}"));
    }
    let token = value.get("value").and_then(Value::as_str).ok_or_else(|| anyhow!("realtime client secret missing value"))?;
    let expires = value.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
    Ok(json!({
        "type": "websocket",
        "url": format!("{}/realtime?model={}", base.replacen("http", "ws", 1), url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>()),
        "token": token,
        "expiresAt": expires,
    }))
}

pub async fn session(input: &Value) -> Result<Value> {
    let model = input.get("model").and_then(Value::as_str).ok_or_else(|| anyhow!("voice model is required"))?;
    let engine = engine(model).ok_or_else(|| anyhow!("unsupported voice model: {model}"))?;
    let transport = input.get("transport").cloned().unwrap_or_else(|| json!({ "type": "websocket" }));
    let kind = transport.get("type").and_then(Value::as_str).unwrap_or("websocket");
    if !engine.transports.contains(&kind) {
        return Err(anyhow!("{model} only supports {} transport", engine.transports.join(", ")));
    }
    let access = source(engine).ok_or_else(|| anyhow!("voice needs an API key for {}", engine.provider))?;
    let mut session = input.get("session").cloned().unwrap_or_else(|| json!({}));
    session["model"] = json!(model);
    match engine.id {
        "openai-live" => {
            let base = access.base.clone().unwrap_or_else(|| OPENAI_BASE.to_string());
            let (status, value) = post_json(&format!("{base}/live/sessions"), &access, json!({ "session": session, "transport": transport })).await?;
            if !(200..300).contains(&status) {
                return Err(anyhow!("live session failed: HTTP {status} {value}"));
            }
            let sdp = value.pointer("/transport/sdp").and_then(Value::as_str).ok_or_else(|| anyhow!("live session missing sdp"))?;
            Ok(json!({ "type": "webrtc", "sdp": sdp }))
        }
        "openai-realtime" if kind == "websocket" => {
            client_secret(OPENAI_BASE, model, &access, json!({ "expires_after": { "anchor": "created_at", "seconds": 60 }, "session": session })).await
        }
        "openai-realtime" => {
            let sdp = transport.get("sdp").and_then(Value::as_str).ok_or_else(|| anyhow!("webrtc voice session needs sdp"))?;
            let form = wreq::multipart::Form::new().text("sdp", sdp.to_string()).text("session", json_text(&session));
            let url = match transport.get("dcid").and_then(Value::as_u64) {
                Some(dcid) => format!("{OPENAI_BASE}/realtime/calls?dcid={dcid}"),
                None => format!("{OPENAI_BASE}/realtime/calls"),
            };
            let builder = crate::http::client()
                .post(url)
                .header("authorization", format!("Bearer {}", access.token))
                .multipart(form);
            let response = crate::http::send(builder.timeout(std::time::Duration::from_secs(30))).await?;
            if !response.status().is_success() {
                return Err(anyhow!("realtime call failed: HTTP {}", response.status().as_u16()));
            }
            let sdp = response.text().await?;
            Ok(json!({ "type": "webrtc", "sdp": sdp }))
        }
        "xai-realtime" => client_secret(XAI_BASE, model, &access, json!({ "expires_after": { "seconds": 60 } })).await,
        "gemini-live" => {
            let now = chrono::Utc::now();
            let builder = crate::http::client()
                .post(format!("{GOOGLE_BASE}/auth_tokens"))
                .header("content-type", "application/json")
                .header("x-goog-api-key", access.token.clone());
            let mut setup = session.clone();
            setup["model"] = json!(format!("models/{model}"));
            let body = json!({
                "uses": 1,
                "expireTime": (now + chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "newSessionExpireTime": (now + chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "bidiGenerateContentSetup": setup,
            });
            let response = crate::http::send(builder.body(json_text(&body)).timeout(std::time::Duration::from_secs(30))).await?;
            if !response.status().is_success() {
                return Err(anyhow!("gemini auth token failed: HTTP {}", response.status().as_u16()));
            }
            let value: Value = response.json().await?;
            let token = value.get("name").and_then(Value::as_str).ok_or_else(|| anyhow!("gemini auth token missing name"))?;
            Ok(json!({
                "type": "websocket",
                "url": "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContentConstrained",
                "token": token,
                "expiresAt": (now + chrono::Duration::minutes(1)).timestamp(),
            }))
        }
        _ => Err(anyhow!("unsupported voice model: {model}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_engine_versions() {
        assert_eq!(engine("gpt-live-1").map(|engine| engine.id), Some("openai-live"));
        assert_eq!(engine("gpt-realtime-2.1").map(|engine| engine.id), Some("openai-realtime"));
        assert_eq!(engine("grok-voice-3.0").map(|engine| engine.id), Some("xai-realtime"));
        assert_eq!(engine("gemini-3.8-live").map(|engine| engine.id), Some("gemini-live"));
        assert!(engine("unknown").is_none());
    }

    #[test]
    fn exposes_transports() {
        let value = models();
        let items = value["models"].as_array().unwrap();
        assert!(items.iter().any(|item| item["model"] == "gpt-realtime-2.1" && item["transports"].as_array().unwrap().len() == 2));
    }
}
