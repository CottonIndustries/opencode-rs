//! Well-known integration sources.
//!
//! A source origin serves `/.well-known/opencode` with an optional inline
//! config and an optional remote config URL. Only the allow-listed config keys
//! are merged; other keys are ignored by config loading.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

fn directory() -> PathBuf {
    crate::paths::get().storage().join("wellknown")
}

fn sources_path() -> PathBuf {
    directory().join("sources.json")
}

fn key(origin: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(origin.as_bytes()))
}

pub fn sources() -> Vec<String> {
    std::fs::read_to_string(sources_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

fn normalize(origin: &str) -> Result<String> {
    let url = url::Url::parse(origin.trim()).map_err(|error| anyhow!("Invalid integration URL: {error}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(anyhow!("Invalid integration URL: expected HTTP(S)"));
    }
    Ok(origin.trim().trim_end_matches('/').to_string())
}

fn substitute(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("{env:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

async fn json(url: &str, headers: &std::collections::BTreeMap<String, String>) -> Result<Value> {
    let mut request = crate::http::client()
        .get(url)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(15));
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = crate::http::send(request).await.with_context(|| format!("fetching {url}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(anyhow!("{url} returned HTTP {status}"));
    }
    Ok(response.json().await?)
}

async fn resolve(origin: &str) -> Result<(Value, Vec<Value>)> {
    let manifest = json(&format!("{origin}/.well-known/opencode"), &Default::default()).await?;
    if !manifest.is_object() {
        return Err(anyhow!("well-known manifest from {origin} must be an object"));
    }
    let mut configs = Vec::new();
    if let Some(config) = manifest.get("config").filter(|config| config.is_object()) {
        configs.push(config.clone());
    }
    if let Some(remote) = manifest.get("remote_config") {
        let Some(url) = remote.get("url").and_then(Value::as_str) else {
            return Err(anyhow!("remote_config.url is required"));
        };
        let headers: std::collections::BTreeMap<String, String> = remote
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), substitute(value))))
            .collect();
        let body = json(&substitute(url), &headers).await?;
        let config = body
            .get("config")
            .filter(|config| config.is_object())
            .cloned()
            .unwrap_or(body);
        if config.is_object() {
            configs.push(config);
        }
    }
    Ok((manifest, configs))
}

/// Discover and persist a source. The manifest is cached so an unreachable
/// source keeps its last resolved config after a restart.
pub async fn add(origin: &str) -> Result<Value> {
    let origin = normalize(origin)?;
    let (manifest, configs) = resolve(&origin).await?;
    std::fs::create_dir_all(directory())?;
    std::fs::write(
        directory().join(format!("{}.json", key(&origin))),
        serde_json::to_vec_pretty(&json!({ "origin": origin, "manifest": manifest, "configs": configs }))?,
    )?;
    let mut all = sources();
    if !all.contains(&origin) {
        all.push(origin.clone());
        all.sort();
        std::fs::write(sources_path(), serde_json::to_vec_pretty(&all)?)?;
    }
    Ok(json!({ "origin": origin, "configs": configs }))
}

pub fn remove(origin: &str) {
    let Ok(origin) = normalize(origin) else { return };
    let mut all = sources();
    all.retain(|value| value != &origin);
    let _ = std::fs::write(sources_path(), serde_json::to_vec_pretty(&all).unwrap_or_default());
    let _ = std::fs::remove_file(directory().join(format!("{}.json", key(&origin))));
}

pub fn configs() -> Vec<Value> {
    let mut out = Vec::new();
    for origin in sources() {
        let Ok(text) = std::fs::read_to_string(directory().join(format!("{}.json", key(&origin)))) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(configs) = value.get("configs").and_then(Value::as_array) {
            out.extend(configs.iter().filter(|config| config.is_object()).cloned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_substitutes() {
        assert_eq!(normalize("https://example.com/").unwrap(), "https://example.com");
        assert!(normalize("file:///tmp/x").is_err());
        std::env::set_var("OPENCODE_WELLKNOWN_TEST", "value");
        assert_eq!(substitute("a-{env:OPENCODE_WELLKNOWN_TEST}-b"), "a-value-b");
    }
}
