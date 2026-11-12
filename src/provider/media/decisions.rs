//! Decision generation protocols.

use super::{cost, decision_base_url, decision_headers, post_json, tokens_json, value_f64, DecisionProtocol};
use crate::models::{Model, Provider};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

pub async fn generate(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    match super::decision_protocol(provider)? {
        DecisionProtocol::OpenAi => openai(provider, model, input).await,
        DecisionProtocol::Cloudflare => cloudflare(provider, model, input).await,
        DecisionProtocol::SystemOne => system_one(provider, model, input).await,
    }
}

fn parts(input: &Value) -> Vec<Value> {
    input.get("input").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn questions(input: &Value) -> Vec<Value> {
    input.get("questions").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn data_url(data: &str, mime: &str) -> String {
    format!("data:{mime};base64,{data}")
}

async fn openai(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    let base = decision_base_url(provider, model, DecisionProtocol::OpenAi);
    let url = format!("{base}/decisions");
    let content: Vec<Value> = parts(input)
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => Some(json!({ "type": "input_text", "text": part.get("text") })),
            Some("json") => {
                let value = part.get("value").cloned().unwrap_or(Value::Null);
                let object = value.is_object()
                    || value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_object));
                if object {
                    Some(json!({ "type": "input_json", "data": value }))
                } else {
                    Some(json!({ "type": "input_text", "text": value.to_string() }))
                }
            }
            Some("file") => {
                let mime = part.get("mime").and_then(Value::as_str).unwrap_or("image/png");
                if !mime.starts_with("image/") {
                    return None;
                }
                Some(json!({
                    "type": "input_image",
                    "image_url": data_url(
                        part.get("data").and_then(Value::as_str).unwrap_or_default(),
                        mime
                    ),
                }))
            }
            _ => None,
        })
        .collect();
    let encoded_questions: Vec<Value> = questions(input)
        .into_iter()
        .map(|question| {
            if question.get("type").and_then(Value::as_str) != Some("predicate") {
                return question;
            }
            let mut next = question;
            let instructions = next.get("instructions").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut meanings = Vec::new();
            if let Some(value) = next.pointer("/criteria/true").and_then(Value::as_str) {
                meanings.push(format!("True means: {value}"));
            }
            if let Some(value) = next.pointer("/criteria/false").and_then(Value::as_str) {
                meanings.push(format!("False means: {value}"));
            }
            if !meanings.is_empty() {
                next["instructions"] = json!(format!("{instructions}\n\n{}", meanings.join("\n")));
            }
            if let Some(object) = next.as_object_mut() {
                object.remove("criteria");
            }
            next
        })
        .collect();
    let body = json!({
        "model": model.id,
        "input": [{ "role": "user", "content": content }],
        "questions": encoded_questions,
    });
    let response = post_json(&url, &decision_headers(provider, model), &body).await?;
    let answers = response.get("answers").and_then(Value::as_array).cloned().unwrap_or_default();
    let answers: Vec<Value> = answers
        .into_iter()
        .map(|answer| {
            if answer.get("type").and_then(Value::as_str) == Some("refusal") {
                let mut value = json!({
                    "type": "error",
                    "name": answer.get("name"),
                    "reason": "refusal",
                });
                if let Some(message) = answer.get("message").filter(|value| !value.is_null()) {
                    value["message"] = message.clone();
                }
                value
            } else {
                answer
            }
        })
        .collect();
    let requested = questions(input);
    let by_name: std::collections::BTreeMap<String, Value> = answers
        .iter()
        .filter_map(|answer| Some((answer.get("name")?.as_str()?.to_string(), answer.clone())))
        .collect();
    let ordered: Vec<Value> = requested
        .iter()
        .filter_map(|question| question.get("name").and_then(Value::as_str))
        .filter_map(|name| by_name.get(name).cloned())
        .collect();
    let answers = if ordered.len() == answers.len() { ordered } else { answers };
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "input_tokens");
    let output_tokens = value_f64(&usage, "output_tokens");
    let mut result = json!({
        "answers": answers,
        "native": { "openai": { "model": response.get("model"), "usage": usage } },
    });
    if input_tokens.is_some() || output_tokens.is_some() {
        result["tokens"] = tokens_json(input_tokens, output_tokens);
    }
    if let Some(cost) = cost(model, input_tokens, output_tokens) {
        result["cost"] = json!(cost);
    }
    Ok(result)
}

async fn system_one(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    let base = decision_base_url(provider, model, DecisionProtocol::SystemOne);
    let placement = if provider.id.eq_ignore_ascii_case("baseten") {
        "state.messages"
    } else if provider.id.eq_ignore_ascii_case("openrouter") {
        "state"
    } else {
        "assets"
    };
    let body = {
        let mut body = system_one_body(input, placement)?;
        body["model"] = json!(model.id);
        body
    };
    let response = post_json(&base, &decision_headers(provider, model), &body).await?;
    let requested = questions(input);
    let answer_map = response
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("System One returned no answers"))?;
    if requested.iter().any(|question| {
        question
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(|name| !answer_map.contains_key(name))
    }) {
        bail!("System One answers do not match the requested questions");
    }
    let answers: Vec<Value> = requested
        .iter()
        .filter_map(|question| {
            let name = question.get("name").and_then(Value::as_str)?;
            let answer = answer_map.get(name)?;
            system_one_answer(question, answer)
        })
        .collect();
    if answers.len() != requested.len() {
        bail!("System One returned an invalid answer");
    }
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "input_tokens");
    let output_tokens = value_f64(&usage, "output_tokens");
    let metadata_key = match provider.id.to_ascii_lowercase().as_str() {
        "opencode" => "opencode",
        "typesafe-ai" => "typesafe",
        "digitalocean" => "digitalocean",
        "baseten" => "baseten",
        "openrouter" => "openrouter",
        _ => provider.id.as_str(),
    };
    let mut native = json!({
        "model": response.get("model"),
        "usage": usage,
        "responseId": response.get("id"),
        "provider": response.get("provider"),
    });
    if let Some(extra) = response
        .pointer(&format!("/provider_metadata/{metadata_key}"))
        .and_then(Value::as_object)
    {
        for (key, value) in extra {
            native[key] = value.clone();
        }
    }
    let mut value = json!({
        "answers": answers,
        "native": { metadata_key: native },
    });
    if input_tokens.is_some() || output_tokens.is_some() {
        value["tokens"] = tokens_json(input_tokens, output_tokens);
    }
    let usage_cost = value_f64(&usage, "cost");
    if let Some(cost) = usage_cost.or_else(|| cost(model, input_tokens, output_tokens)) {
        value["cost"] = json!(cost);
    }
    Ok(value)
}

async fn cloudflare(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    let base = decision_base_url(provider, model, DecisionProtocol::Cloudflare);
    let body = json!({
        "model": model.id,
        "input": system_one_body(input, "root")?,
    });
    let response = post_json(&base, &decision_headers(provider, model), &body).await?;
    let result = response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("Cloudflare Run returned no result"))?;
    let result = if result.get("state").and_then(Value::as_str) == Some("Completed") {
        result.get("result").cloned().ok_or_else(|| anyhow!("Cloudflare Run returned no completed result"))?
    } else {
        result
    };
    let requested = questions(input);
    let answer_map = result.get("answers").and_then(Value::as_object).cloned().unwrap_or_default();
    if answer_map.len() != requested.len()
        || requested.iter().any(|question| {
            question
                .get("name")
                .and_then(Value::as_str)
                .is_none_or(|name| !answer_map.contains_key(name))
        })
    {
        bail!("Cloudflare Run answers do not match the requested questions");
    }
    let answers: Vec<Value> = requested
        .iter()
        .filter_map(|question| {
            let name = question.get("name").and_then(Value::as_str)?;
            let answer = answer_map.get(name)?;
            system_one_answer(question, answer)
        })
        .collect();
    let usage = result.get("usage").cloned().unwrap_or(Value::Null);
    let input_tokens = value_f64(&usage, "input_tokens");
    let output_tokens = value_f64(&usage, "output_tokens");
    let mut value = json!({
        "answers": answers,
        "native": { "cloudflare-workers-ai": {
            "model": result.get("model"),
            "usage": usage,
            "responseId": result.get("id"),
            "provider": result.get("provider"),
        } },
    });
    if input_tokens.is_some() || output_tokens.is_some() {
        value["tokens"] = tokens_json(input_tokens, output_tokens);
    }
    let usage_cost = value_f64(&usage, "cost");
    if let Some(cost) = usage_cost.or_else(|| cost(model, input_tokens, output_tokens)) {
        value["cost"] = json!(cost);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelKind, Provider};
    use std::collections::BTreeMap;

    fn test_provider(id: &str, npm: &str, api: String) -> Provider {
        let mut options = crate::config::ProviderOptions::default();
        options.base_url = Some(api);
        options.api_key = Some("test".into());
        Provider {
            id: id.into(),
            name: id.into(),
            npm: npm.into(),
            api: None,
            env: vec![],
            key: Some("test".into()),
            options,
            models: BTreeMap::new(),
            media_models: BTreeMap::new(),
        }
    }

    fn test_model(provider_id: &str) -> crate::models::Model {
        crate::models::Model {
            id: "decision-model".into(),
            provider_id: provider_id.into(),
            kind: ModelKind::Decision,
            name: "decision-model".into(),
            family: String::new(),
            reasoning: false,
            reasoning_options: vec![],
            tool_call: false,
            attachment: false,
            temperature: false,
            context: 0,
            output: 0,
            cost_input: 1.0,
            cost_output: 2.0,
            cost_cache_read: 0.0,
            cost_cache_write: 0.0,
            input_modalities: vec!["text".into()],
            output_modalities: vec!["text".into()],
            interleaved_field: None,
            release_date: String::new(),
            status: None,
            npm: Some("@ai-sdk/openai".into()),
            api: None,
            shape: None,
            options: Value::Null,
            headers: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn openai_decisions_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("/decisions"));
            assert!(request.contains("\"questions\""));
            let body = "{\"model\":\"decision-model\",\"answers\":[{\"type\":\"predicate\",\"name\":\"ok\",\"probability\":0.75}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = test_provider("openai", "@ai-sdk/openai", format!("http://{address}/v1"));
        let model = test_model("openai");
        let input = json!({
            "input": [{ "type": "text", "text": "evidence" }],
            "questions": [{ "type": "predicate", "name": "ok", "instructions": "is it ok" }]
        });
        let value = generate(&provider, &model, &input).await.unwrap();
        assert_eq!(value["answers"][0]["probability"], 0.75);
        assert_eq!(value["tokens"]["input"], 10.0);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cloudflare_decisions_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("\"model\":\"decision-model\""));
            let body = "{\"result\":{\"model\":\"decision-model\",\"answers\":{\"ok\":{\"type\":\"noul\",\"noul\":0.25}},\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"cost\":0.01}}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = test_provider(
            "cloudflare-workers-ai",
            "@ai-sdk/openai-compatible",
            format!("http://{address}/ai/run"),
        );
        let input = json!({
            "input": [{ "type": "text", "text": "evidence" }],
            "questions": [{ "type": "predicate", "name": "ok", "instructions": "is it ok" }]
        });
        let value = generate(&provider, &test_model("cloudflare-workers-ai"), &input).await.unwrap();
        assert_eq!(value["answers"][0]["probability"], 0.25);
        assert_eq!(value["cost"], 0.01);
        server.await.unwrap();
    }

    #[test]
    fn system_one_root_media_and_questions() {
        let input = json!({
            "input": [
                { "type": "text", "text": "evidence" },
                { "type": "file", "data": "abc", "mime": "image/png" }
            ],
            "questions": [
                { "type": "predicate", "name": "ok", "instructions": "is it ok", "criteria": { "true": "yes" } },
                { "type": "choice", "name": "pick", "instructions": "pick", "choices": [
                    { "value": true, "description": "yes" },
                    { "value": false }
                ] }
            ]
        });
        let body = system_one_body(&input, "root").unwrap();
        assert_eq!(body["state"], "evidence");
        assert_eq!(body["images"][0], "data:image/png;base64,abc");
        assert_eq!(body["questions"]["ok"]["type"], "noul");
        assert_eq!(body["questions"]["pick"]["criteria"]["true"], "yes");
        assert_eq!(body["questions"]["pick"]["criteria"]["false"], Value::Null);
    }

    #[test]
    fn system_one_assets_placement_replaces_media_with_references() {
        let input = json!({
            "input": [
                { "type": "text", "text": "look" },
                { "type": "file", "data": "abc", "mime": "image/jpeg" }
            ],
            "questions": []
        });
        let body = system_one_body(&input, "assets").unwrap();
        assert_eq!(body["state"], json!(["look", "<image-1>"]));
        assert_eq!(body["assets"]["<image-1>"]["format"], "jpeg");
        assert_eq!(body["assets"]["<image-1>"]["data_base64"], "abc");
    }

    #[test]
    fn system_one_state_messages_placement_builds_chat_content() {
        let input = json!({
            "input": [
                { "type": "text", "text": "look" },
                { "type": "file", "data": "abc", "mime": "image/png" }
            ],
            "questions": []
        });
        let body = system_one_body(&input, "state.messages").unwrap();
        assert_eq!(body["state"]["messages"][0]["role"], "user");
        assert_eq!(body["state"]["messages"][0]["content"][0]["text"], "look");
        assert_eq!(
            body["state"]["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
    }

    #[test]
    fn system_one_state_placement_keeps_media_in_state() {
        let input = json!({
            "input": [{ "type": "file", "data": "abc", "mime": "image/png" }],
            "questions": []
        });
        let body = system_one_body(&input, "state").unwrap();
        assert_eq!(body["state"][0]["image_url"]["url"], "data:image/png;base64,abc");
    }

    #[tokio::test]
    async fn typesafe_decisions_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("/systemone"));
            assert!(request.contains("\"model\":\"decision-model\""));
            let body = "{\"model\":\"decision-model\",\"answers\":{\"ok\":{\"type\":\"noul\",\"noul\":0.75}},\"usage\":{\"input_tokens\":3,\"output_tokens\":4},\"provider_metadata\":{\"typesafe\":{\"trace\":\"x\"}}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = test_provider(
            "typesafe-ai",
            "@ai-sdk/openai-compatible",
            format!("http://{address}/v1"),
        );
        let input = json!({
            "input": [{ "type": "text", "text": "evidence" }],
            "questions": [{ "type": "predicate", "name": "ok", "instructions": "is it ok" }]
        });
        let value = generate(&provider, &test_model("typesafe-ai"), &input).await.unwrap();
        assert_eq!(value["answers"][0]["probability"], 0.75);
        assert_eq!(value["native"]["typesafe"]["trace"], "x");
        assert_eq!(value["tokens"]["input"], 3.0);
        server.await.unwrap();
    }

    #[test]
    fn system_one_choice_answer_maps_boolean() {
        let question = json!({
            "name": "pick",
            "type": "choice",
            "choices": [{ "value": true }, { "value": false }]
        });
        let answer = json!({
            "type": "choice",
            "choice": "true",
            "probabilities": { "false": 0.2, "true": 0.8 },
            "confidence": 0.8
        });
        let value = system_one_answer(&question, &answer).unwrap();
        assert_eq!(value["choice"], true);
        assert_eq!(value["probabilities"][0]["value"], true);
    }
}

fn system_one_body(input: &Value, placement: &str) -> Result<Value> {
    let mut encoded_questions = Map::new();
    for question in questions(input) {
        let Some(name) = question.get("name").and_then(Value::as_str) else { continue };
        let instructions = question.get("instructions").and_then(Value::as_str).unwrap_or_default();
        match question.get("type").and_then(Value::as_str) {
            Some("predicate") => {
                let mut value = json!({ "type": "noul", "instructions": instructions });
                if let Some(criteria) = question.get("criteria") {
                    value["criteria"] = criteria.clone();
                }
                encoded_questions.insert(name.to_string(), value);
            }
            Some("score") => {
                let criteria: Vec<Value> = question
                    .get("levels")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|level| {
                        let label = level.get("label").and_then(Value::as_str).unwrap_or_default();
                        match level.get("description").and_then(Value::as_str) {
                            Some(description) => json!(format!("{label}: {description}")),
                            None => json!(label),
                        }
                    })
                    .collect();
                encoded_questions.insert(
                    name.to_string(),
                    json!({ "type": "score", "instructions": instructions, "criteria": criteria }),
                );
            }
            Some("choice") => {
                let choices = question.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut keys = Vec::new();
                let mut criteria = Map::new();
                for choice in choices {
                    let key = match choice.get("value") {
                        Some(Value::String(value)) => value.clone(),
                        Some(Value::Bool(value)) => value.to_string(),
                        _ => continue,
                    };
                    if keys.contains(&key) {
                        bail!("System One choice options are strings; \"{name}\" has duplicate option values");
                    }
                    criteria.insert(
                        key.clone(),
                        choice.get("description").cloned().unwrap_or(Value::Null),
                    );
                    keys.push(key);
                }
                encoded_questions.insert(
                    name.to_string(),
                    json!({ "type": "choice", "instructions": instructions, "criteria": criteria }),
                );
            }
            _ => {}
        }
    }

    let parts = parts(input);
    let has_media = parts.iter().any(|part| part.get("type").and_then(Value::as_str) == Some("file"));
    let mut plain = Vec::new();
    let mut content = Vec::new();
    let mut assets = Map::new();
    let mut root: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    for part in &parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = part.get("text").cloned().unwrap_or(Value::Null);
                plain.push(text.clone());
                content.push(json!({ "type": "text", "text": text }));
            }
            Some("json") => {
                let value = part.get("value").cloned().unwrap_or(Value::Null);
                plain.push(value.clone());
                let text = match value {
                    Value::String(text) => text,
                    value => value.to_string(),
                };
                content.push(json!({ "type": "text", "text": text }));
            }
            Some("file") => {
                let mime = part.get("mime").and_then(Value::as_str).unwrap_or("image/png");
                let data = part.get("data").and_then(Value::as_str).unwrap_or_default();
                let url = data_url(data, mime);
                match placement {
                    "assets" => {
                        if !mime.starts_with("image/") {
                            bail!("System One assets placement takes images; got {mime}");
                        }
                        let reference = format!("<image-{}>", assets.len() + 1);
                        assets.insert(
                            reference.clone(),
                            json!({
                                "type": "image",
                                "format": mime.trim_start_matches("image/"),
                                "data_base64": data,
                            }),
                        );
                        plain.push(json!(reference));
                        content.push(json!({ "type": "text", "text": reference }));
                    }
                    "state" => {
                        let item = system_one_state_item(mime, data, &url)?;
                        content.push(item);
                    }
                    "state.messages" => {
                        let item = system_one_state_item(mime, data, &url)?;
                        content.push(item);
                    }
                    _ => {
                        let kind = if mime.starts_with("image/") {
                            "images"
                        } else if mime.starts_with("audio/") {
                            "audio"
                        } else if mime.starts_with("video/") {
                            "videos"
                        } else {
                            bail!("System One takes images, audio and video; got {mime}");
                        };
                        root.entry(kind).or_default().push(url);
                    }
                }
            }
            _ => {}
        }
    }
    let state = if placement == "state" && has_media {
        Value::Array(content)
    } else if placement == "state.messages" && has_media {
        json!({ "messages": [{ "role": "user", "content": content }] })
    } else {
        match plain.len() {
            0 => Value::Array(Vec::new()),
            1 => plain.remove(0),
            _ => Value::Array(plain),
        }
    };
    let mut body = json!({ "state": state, "questions": Value::Object(encoded_questions) });
    if placement == "assets" && !assets.is_empty() {
        body["assets"] = Value::Object(assets);
    }
    for (kind, items) in root {
        body[kind] = json!(items);
    }
    Ok(body)
}

fn system_one_state_item(mime: &str, data: &str, url: &str) -> Result<Value> {
    if mime.starts_with("image/") {
        return Ok(json!({ "type": "image_url", "image_url": { "url": url } }));
    }
    if mime.starts_with("video/") {
        return Ok(json!({ "type": "video_url", "video_url": { "url": url } }));
    }
    if mime.starts_with("audio/") {
        return Ok(json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": mime.trim_start_matches("audio/") },
        }));
    }
    bail!("System One takes images, audio and video; got {mime}")
}

fn system_one_answer(question: &Value, answer: &Value) -> Option<Value> {
    let name = question.get("name").and_then(Value::as_str)?;
    match answer.get("type").and_then(Value::as_str) {
        Some("noul") => Some(json!({
            "name": name,
            "type": "predicate",
            "probability": answer.get("noul"),
        })),
        Some("choice") => {
            let choices = question.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
            let value_for = |key: &str| {
                choices
                    .iter()
                    .find(|choice| match choice.get("value") {
                        Some(Value::String(value)) => value == key,
                        Some(Value::Bool(value)) => value.to_string() == key,
                        _ => false,
                    })
                    .and_then(|choice| choice.get("value"))
                    .cloned()
                    .unwrap_or_else(|| json!(key))
            };
            let rank = |key: &str| {
                choices
                    .iter()
                    .position(|choice| match choice.get("value") {
                        Some(Value::String(value)) => value == key,
                        Some(Value::Bool(value)) => value.to_string() == key,
                        _ => false,
                    })
                    .unwrap_or(choices.len())
            };
            let choice = answer.get("choice").and_then(Value::as_str).unwrap_or_default();
            let mut probabilities: Vec<(String, f64)> = answer
                .get("probabilities")
                .and_then(Value::as_object)
                .map(|values| {
                    values
                        .iter()
                        .map(|(key, value)| (key.clone(), value.as_f64().unwrap_or(0.0)))
                        .collect()
                })
                .unwrap_or_default();
            probabilities.sort_by_key(|(key, _)| rank(key));
            Some(json!({
                "name": name,
                "type": "choice",
                "choice": value_for(choice),
                "probabilities": probabilities.iter().map(|(key, probability)| {
                    json!({ "value": value_for(key), "probability": probability })
                }).collect::<Vec<_>>(),
                "confidence": answer.get("confidence"),
            }))
        }
        Some("score") => {
            let levels = question.get("levels").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut probabilities: Vec<(i64, f64)> = answer
                .get("probabilities")
                .and_then(Value::as_object)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|(key, value)| Some((key.parse::<i64>().ok()?, value.as_f64().unwrap_or(0.0))))
                        .collect()
                })
                .unwrap_or_default();
            probabilities.sort_by_key(|(value, _)| *value);
            Some(json!({
                "name": name,
                "type": "score",
                "score": answer.get("score"),
                "probabilities": probabilities.iter().map(|(value, probability)| {
                    let label = levels
                        .get(*value as usize)
                        .and_then(|level| level.get("label"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    json!({ "value": value, "label": label, "probability": probability })
                }).collect::<Vec<_>>(),
                "confidence": answer.get("confidence"),
            }))
        }
        _ => None,
    }
}
