//! Image and decision generation protocols, ported from opencode's
//! TypeScript provider protocols and served with the same Bun-compatible HTTP
//! client as the chat providers.

pub mod decisions;
pub mod images;
pub mod sizing;

use crate::models::{Model, Provider};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct ImageFile {
    pub data: String,
    pub mime: String,
}

#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub prompt: String,
    pub images: Vec<ImageFile>,
    pub n: Option<u32>,
    pub aspect: Option<String>,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageOutput {
    pub data: String,
    pub mime: String,
    pub name: Option<String>,
}

#[derive(Debug, Default)]
pub struct ImageResult {
    pub images: Vec<ImageOutput>,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub cost: Option<f64>,
    pub native: Option<Value>,
}

impl ImageResult {
    pub fn json(&self) -> Value {
        let mut value = json!({
            "images": self.images.iter().map(|image| {
                let mut item = json!({ "type": "image", "data": image.data, "mime": image.mime });
                if let Some(name) = &image.name {
                    item["name"] = json!(name);
                }
                item
            }).collect::<Vec<_>>(),
        });
        if self.input_tokens.is_some() || self.output_tokens.is_some() {
            value["tokens"] = tokens_json(self.input_tokens, self.output_tokens);
        }
        if let Some(cost) = self.cost {
            value["cost"] = json!(cost);
        }
        if let Some(native) = &self.native {
            value["native"] = native.clone();
        }
        value
    }
}

pub fn tokens_json(input: Option<f64>, output: Option<f64>) -> Value {
    let mut tokens = Map::new();
    if let Some(input) = input {
        tokens.insert("input".into(), json!(input));
    }
    if let Some(output) = output {
        tokens.insert("output".into(), json!(output));
    }
    Value::Object(tokens)
}

pub fn cost(model: &Model, input: Option<f64>, output: Option<f64>) -> Option<f64> {
    if input.is_none() && output.is_none() {
        return None;
    }
    Some((
        input.unwrap_or(0.0) * model.cost_input
            + output.unwrap_or(0.0) * model.cost_output
    ) / 1_000_000.0)
}

pub fn parse_image_files(value: Option<&Value>) -> Vec<ImageFile> {
    value
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    Some(ImageFile {
                        data: file.get("data")?.as_str()?.to_string(),
                        mime: file.get("mime")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn image_request(input: &Value) -> ImageRequest {
    ImageRequest {
        prompt: input.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string(),
        images: parse_image_files(input.get("images")),
        n: input.get("n").and_then(Value::as_u64).map(|n| n as u32),
        aspect: input.get("aspect").and_then(Value::as_str).map(String::from),
        resolution: input.get("resolution").and_then(Value::as_str).map(String::from),
    }
}

pub async fn generate_image(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    let request = image_request(input);
    let result = images::generate(provider, model, &request).await?;
    Ok(result.json())
}

pub async fn generate_decisions(provider: &Provider, model: &Model, input: &Value) -> Result<Value> {
    decisions::generate(provider, model, input).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    OpenAi,
    Google,
    Xai,
    Zai,
    Alibaba,
    Meta,
    Minimax,
    Cloudflare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionProtocol {
    OpenAi,
    Cloudflare,
}

pub fn image_protocol(provider: &Provider) -> Result<ImageProtocol> {
    let id = provider.id.to_ascii_lowercase();
    let npm = provider.npm.to_ascii_lowercase();
    if id.starts_with("alibaba") {
        return Ok(ImageProtocol::Alibaba);
    }
    if id.starts_with("minimax") {
        return Ok(ImageProtocol::Minimax);
    }
    if id.starts_with("cloudflare-workers-ai") {
        return Ok(ImageProtocol::Cloudflare);
    }
    if id == "meta" {
        return Ok(ImageProtocol::Meta);
    }
    if id == "zai" || id == "zhipuai" || id == "zai-coding-plan" || id == "zhipuai-coding-plan" {
        return Ok(ImageProtocol::Zai);
    }
    if id == "xai" {
        return Ok(ImageProtocol::Xai);
    }
    if id == "google" || id == "google-vertex" || npm.contains("google") {
        return Ok(ImageProtocol::Google);
    }
    if npm.contains("xai") {
        return Ok(ImageProtocol::Xai);
    }
    if id == "openai"
        || id.starts_with("azure")
        || id == "openrouter"
        || id == "opencode"
        || id == "vercel-ai-gateway"
        || npm.contains("openai")
        || npm.contains("openrouter")
        || npm.contains("azure")
        || npm.contains("gateway")
    {
        return Ok(ImageProtocol::OpenAi);
    }
    Err(anyhow!("provider {} has no image protocol", provider.id))
}

pub fn decision_protocol(provider: &Provider) -> Result<DecisionProtocol> {
    let id = provider.id.to_ascii_lowercase();
    if id.starts_with("cloudflare") {
        return Ok(DecisionProtocol::Cloudflare);
    }
    let npm = provider.npm.to_ascii_lowercase();
    if matches!(id.as_str(), "openai" | "openrouter" | "vercel-ai-gateway")
        || id.starts_with("azure")
        || npm.contains("openai")
        || npm.contains("openrouter")
        || npm.contains("azure")
        || npm.contains("gateway")
    {
        return Ok(DecisionProtocol::OpenAi);
    }
    Err(anyhow!("provider {} has no decision protocol", provider.id))
}

pub fn base_url(provider: &Provider, model: &Model, default: &str, protocol: ImageProtocol) -> String {
    let explicit = provider.options.base_url.is_some();
    let mut url = provider
        .options
        .base_url
        .clone()
        .or_else(|| provider.base_url(model))
        .unwrap_or_else(|| default.to_string());
    url = url.trim_end_matches('/').to_string();
    if explicit {
        return url;
    }
    match protocol {
        ImageProtocol::Alibaba => {
            if url.contains("/compatible-mode/") {
                url = url.replace("/compatible-mode/", "/");
            }
            if !url.ends_with("/api/v1") && url.ends_with("/v1") {
                url = format!("{}/api/v1", url.trim_end_matches("/v1"));
            }
        }
        ImageProtocol::Minimax => {
            if let Some(next) = url.strip_suffix("/anthropic/v1") {
                url = format!("{next}/v1");
            }
        }
        ImageProtocol::Cloudflare => {
            if !url.ends_with("/ai/run") {
                if let Some(next) = url.strip_suffix("/ai/v1") {
                    url = format!("{next}/ai/run");
                } else if url.ends_with("/v1") {
                    url = format!("{}/run", url.trim_end_matches("/v1"));
                }
            }
        }
        _ => {}
    }
    url
}

pub fn decision_base_url(provider: &Provider, model: &Model, protocol: DecisionProtocol) -> String {
    let url = provider
        .options
        .base_url
        .clone()
        .or_else(|| provider.base_url(model))
        .unwrap_or_else(|| match protocol {
            DecisionProtocol::OpenAi => "https://api.openai.com/v1".into(),
            DecisionProtocol::Cloudflare => "https://api.cloudflare.com/client/v4".into(),
        });
    let url = url.trim_end_matches('/').to_string();
    match protocol {
        DecisionProtocol::Cloudflare if url.ends_with("/ai/v1") => {
            format!("{}/run", url.trim_end_matches("/v1"))
        }
        _ => url,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AuthStyle {
    Bearer,
    Google,
    Azure,
}

pub fn headers(provider: &Provider, model: &Model, style: AuthStyle) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = provider
        .options
        .headers
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in &model.headers {
        if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case(key)) {
            headers.push((key.clone(), value.clone()));
        }
    }
    if let Some(key) = &provider.key {
        match style {
            AuthStyle::Bearer => {
                if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("authorization")) {
                    headers.push(("authorization".into(), format!("Bearer {key}")));
                }
            }
            AuthStyle::Azure => {
                if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("api-key")) {
                    headers.push(("api-key".into(), key.clone()));
                }
            }
            AuthStyle::Google => {
                if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("x-goog-api-key")) {
                    headers.push(("x-goog-api-key".into(), key.clone()));
                }
            }
        }
    }
    if provider.id == "openrouter" {
        headers.push(("HTTP-Referer".into(), "https://opencode.ai/".into()));
        headers.push(("X-Title".into(), "opencode".into()));
    }
    headers
}

fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, value| !value.is_null());
            for value in map.values_mut() {
                drop_nulls(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                drop_nulls(item);
            }
        }
        _ => {}
    }
}

pub fn decision_headers(provider: &Provider, model: &Model) -> Vec<(String, String)> {
    let style = if provider.id.starts_with("azure") { AuthStyle::Azure } else { AuthStyle::Bearer };
    headers(provider, model, style)
}

pub async fn post_json(url: &str, headers: &[(String, String)], body: &Value) -> Result<Value> {
    let mut body = body.clone();
    drop_nulls(&mut body);
    let mut builder = crate::http::client().post(url).json(&body);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = crate::http::send(builder).await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!("provider request failed ({status}): {}", crate::util::truncate_chars(text.trim(), 500));
    }
    serde_json::from_str(&text).with_context(|| format!("provider returned invalid JSON: {}", crate::util::truncate_chars(&text, 300)))
}

pub async fn post_multipart(url: &str, headers: &[(String, String)], form: wreq::multipart::Form) -> Result<Value> {
    let mut builder = crate::http::client().post(url).multipart(form);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = crate::http::send(builder).await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!("provider request failed ({status}): {}", crate::util::truncate_chars(text.trim(), 500));
    }
    serde_json::from_str(&text).with_context(|| format!("provider returned invalid JSON: {}", crate::util::truncate_chars(&text, 300)))
}

pub async fn post_response(url: &str, headers: &[(String, String)], body: &Value) -> Result<wreq::Response> {
    let mut body = body.clone();
    drop_nulls(&mut body);
    let mut builder = crate::http::client().post(url).json(&body);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = crate::http::send(builder).await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await?;
        bail!("provider request failed ({status}): {}", crate::util::truncate_chars(text.trim(), 500));
    }
    Ok(response)
}

pub fn decode_base64(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .context("provider returned invalid base64 data")
}

pub fn encode_base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub async fn download(url: &str, fallback_mime: &str) -> Result<ImageOutput> {
    if url.starts_with("data:") {
        return parse_data_url(url).ok_or_else(|| anyhow!("provider returned an invalid data URL"));
    }
    let response = crate::http::send(crate::http::client().get(url)).await?;
    if !response.status().is_success() {
        bail!("provider image download failed ({})", response.status());
    }
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_mime.to_string());
    let bytes = response.bytes().await?;
    Ok(ImageOutput { data: encode_base64(&bytes), mime, name: None })
}

pub fn parse_data_url(url: &str) -> Option<ImageOutput> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let (mime, encoding) = meta.split_once(';').unwrap_or((meta, ""));
    if encoding != "base64" {
        return None;
    }
    Some(ImageOutput { data: data.to_string(), mime: mime.to_string(), name: None })
}

pub fn image_output(b64: Option<&str>, url: Option<&str>, fallback_mime: &str) -> Option<ImageOutput> {
    if let Some(b64) = b64.filter(|value| !value.is_empty()) {
        return Some(ImageOutput { data: b64.to_string(), mime: fallback_mime.to_string(), name: None });
    }
    url.filter(|value| !value.is_empty()).and_then(parse_data_url)
}

pub fn mime_for_format(format: Option<&str>) -> String {
    format!("image/{}", format.unwrap_or("png"))
}

pub fn value_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_and_formats() {
        let output = parse_data_url("data:image/png;base64,abc").unwrap();
        assert_eq!(output.mime, "image/png");
        assert_eq!(output.data, "abc");
        assert_eq!(mime_for_format(Some("webp")), "image/webp");
        assert_eq!(mime_for_format(None), "image/png");
    }
}
