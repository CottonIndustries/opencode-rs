//! OpenCode Console workspace sync.
//!
//! After the device login the Console exposes the workspace configuration,
//! instructions and skills. They are cached under `storage/console` so startup
//! is offline and local configuration can still override the hosted one.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

fn root() -> PathBuf {
    crate::paths::get().storage().join("console")
}

fn cache_path() -> PathBuf {
    root().join("cache.json")
}

pub fn skills_dir() -> PathBuf {
    root().join("skills")
}

fn credential() -> Option<(String, String, Option<String>)> {
    let crate::auth::Credential::Oauth { access, metadata, .. } = crate::auth::get("opencode")? else {
        return None;
    };
    let metadata = metadata?;
    let server = metadata
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or("https://opencode.ai/console")
        .trim_end_matches('/')
        .to_string();
    let org = metadata.get("orgID").and_then(Value::as_str).map(String::from);
    Some((server, access, org))
}

async fn fetch(server: &str, access: &str, org: Option<&str>, path: &str) -> Result<Option<Value>> {
    let url = format!("{server}{path}");
    let mut request = crate::http::client()
        .get(&url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {access}"))
        .timeout(std::time::Duration::from_secs(15));
    if let Some(org) = org {
        request = request.header("x-org-id", org);
    }
    let response = crate::http::send(request).await.with_context(|| format!("fetching {url}"))?;
    let status = response.status().as_u16();
    if status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        return Err(anyhow!("{url} returned HTTP {status}"));
    }
    Ok(Some(response.json().await?))
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

fn safe_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.contains(char::from(92))
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn convert_config(remote: &Value) -> Value {
    let mut config = serde_json::Map::new();
    if let Some(providers) = remote.get("providers").and_then(Value::as_object) {
        let mut converted = serde_json::Map::new();
        for (id, provider) in providers {
            let mut options = provider
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(headers) = provider.get("headers").and_then(Value::as_object) {
                options.insert("headers".into(), Value::Object(headers.clone()));
            }
            let mut value = serde_json::Map::new();
            if let Some(name) = provider.get("name") {
                value.insert("name".into(), name.clone());
            }
            if let Some(package) = provider.get("package").or_else(|| provider.get("npm")) {
                value.insert("npm".into(), package.clone());
            }
            if let Some(models) = provider.get("models") {
                value.insert("models".into(), models.clone());
            }
            if !options.is_empty() {
                value.insert("options".into(), Value::Object(options));
            }
            converted.insert(id.clone(), Value::Object(value));
        }
        config.insert("provider".into(), Value::Object(converted));
    }
    for key in ["mcp", "policies", "websearch"] {
        if let Some(value) = remote.get(key) {
            config.insert(key.into(), value.clone());
        }
    }
    Value::Object(config)
}

/// Fetch and cache the active Console workspace. A missing Console config is
/// treated as an empty workspace rather than an error.
pub async fn sync() -> Result<Value> {
    let Some((server, access, org)) = credential() else {
        return Err(anyhow!("not signed in to OpenCode Console"));
    };
    let remote = fetch(&server, &access, org.as_deref(), "/api/v2/config").await?;
    let config = remote.as_ref().map(convert_config).unwrap_or_else(|| json!({}));
    let instructions = fetch(&server, &access, org.as_deref(), "/api/v2/instructions")
        .await?
        .and_then(|value| value.get("instructions").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let listed = fetch(&server, &access, org.as_deref(), "/api/v2/skills")
        .await?
        .and_then(|value| value.get("skills").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let mut skills = Vec::new();
    for item in listed {
        let Some(slug) = item.get("slug").and_then(Value::as_str) else { continue };
        if !safe_segment(slug) {
            continue;
        }
        let path = format!("/api/v2/skills/{}", slug);
        let Some(bundle) = fetch(&server, &access, org.as_deref(), &path).await? else { continue };
        if bundle.get("slug").and_then(Value::as_str) != Some(slug) {
            continue;
        }
        let files: Vec<Value> = bundle
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter(|file| {
                        file.get("path").and_then(Value::as_str).is_some_and(safe_path)
                            && file.get("content").and_then(Value::as_str).is_some()
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !files.iter().any(|file| file.get("path").and_then(Value::as_str) == Some("SKILL.md")) {
            continue;
        }
        skills.push(json!({ "slug": slug, "files": files }));
    }
    let cache = json!({
        "server": server,
        "syncedAt": crate::util::now_ms(),
        "config": config,
        "instructions": instructions,
        "skills": skills,
    });
    std::fs::create_dir_all(root())?;
    std::fs::write(cache_path(), serde_json::to_vec_pretty(&cache)?)?;
    write_skills(&skills)?;
    Ok(json!({
        "server": cache["server"],
        "providers": cache.pointer("/config/provider").and_then(Value::as_object).map(|map| map.len()).unwrap_or(0),
        "instructions": cache["instructions"].as_array().map(Vec::len).unwrap_or(0),
        "skills": cache["skills"].as_array().map(Vec::len).unwrap_or(0),
    }))
}

fn write_skills(skills: &[Value]) -> Result<()> {
    let root = skills_dir();
    let _ = std::fs::remove_dir_all(&root);
    for skill in skills {
        let Some(slug) = skill.get("slug").and_then(Value::as_str) else { continue };
        if !safe_segment(slug) {
            continue;
        }
        let Some(files) = skill.get("files").and_then(Value::as_array) else { continue };
        for file in files {
            let (Some(path), Some(content)) = (
                file.get("path").and_then(Value::as_str),
                file.get("content").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !safe_path(path) {
                continue;
            }
            let target = root.join(slug).join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, content)?;
        }
    }
    Ok(())
}

fn cache() -> Option<Value> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The hosted config layer, in the shape this port's config loader accepts.
pub fn configs() -> Vec<Value> {
    cache()
        .and_then(|cache| cache.get("config").filter(|config| config.is_object()).cloned())
        .into_iter()
        .collect()
}

pub fn instructions() -> Vec<(String, String)> {
    cache()
        .and_then(|cache| cache.get("instructions").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            Some((
                item.get("id").and_then(Value::as_str)?.to_string(),
                item.get("content").and_then(Value::as_str)?.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_console_providers() {
        let config = convert_config(&json!({
            "providers": {
                "acme": {
                    "name": "Acme",
                    "package": "@ai-sdk/openai-compatible",
                    "settings": { "baseURL": "https://example.com/v1", "apiKey": "secret" },
                    "models": { "fast": { "name": "Fast" } }
                }
            },
            "policies": [{ "action": "edit", "resource": "*", "effect": "deny" }]
        }));
        assert_eq!(config["provider"]["acme"]["options"]["baseURL"], "https://example.com/v1");
        assert_eq!(config["provider"]["acme"]["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(config["policies"][0]["action"], "edit");
    }

    #[test]
    fn rejects_escaping_skill_paths() {
        assert!(safe_path("SKILL.md"));
        assert!(safe_path("references/a.md"));
        assert!(!safe_path("../outside.md"));
        assert!(!safe_path("/abs.md"));
        assert!(!safe_path("a\\b.md"));
    }
}
