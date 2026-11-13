//! Package plugin inventory and update checks.
//!
//! Server plugins are loaded by Deno from their configured npm specifier.
//! Checking compares an explicitly pinned version with the registry's latest
//! release; updating rewrites the configured specifier in place.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct Item {
    pub runtime: &'static str,
    pub target: String,
    pub name: String,
    pub version: Option<String>,
    pub latest: Option<String>,
    pub outdated: bool,
    pub error: Option<String>,
    pub loaded: Option<Value>,
}

fn package(spec: &str) -> (String, Option<String>) {
    let rest = spec.strip_prefix("npm:").unwrap_or(spec);
    if let Some(at) = rest.rfind('@') {
        if at > 0 && !rest[at - 1..].starts_with('@') {
            return (rest[..at].to_string(), Some(rest[at + 1..].to_string()));
        }
    }
    (rest.to_string(), None)
}

fn local(spec: &str) -> bool {
    spec.starts_with("file:")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with(".\\")
        || spec.starts_with("..\\")
        || Path::new(spec).is_absolute()
}

fn exact(version: &str) -> bool {
    version
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
        && !version.contains(['^', '~', '*', '<', '>', '|', ' '])
}

async fn latest(name: &str) -> Result<String> {
    let encoded = name.replace('/', "%2F");
    let url = format!("https://registry.npmjs.org/{encoded}/latest");
    let response = crate::http::send(
        crate::http::client()
            .get(&url)
            .header("accept", "application/json")
            .timeout(std::time::Duration::from_secs(15)),
    )
    .await
    .with_context(|| format!("checking {name}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(anyhow!("registry returned HTTP {status} for {name}"));
    }
    let value: Value = response.json().await?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("registry response for {name} has no version"))
}

fn loaded_by_name(app: &crate::app::App) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    for plugin in app.plugins.list() {
        let Some(source) = plugin.get("source") else { continue };
        if source.get("type").and_then(Value::as_str) != Some("package") {
            continue;
        }
        let Some(target) = source.get("target").and_then(Value::as_str) else { continue };
        out.insert(target.to_string(), plugin.clone());
    }
    out
}

fn configured(directory: &Path, worktree: &Path) -> Result<Vec<crate::plugin::PluginSpec>> {
    let mut specs = crate::config::load(directory, worktree)?.plugins;
    specs.retain(|spec| !local(&spec.target));
    Ok(specs)
}

pub async fn inspect(app: &crate::app::App, target: Option<&str>) -> Result<Vec<Item>> {
    let specs = configured(&app.directory, &app.worktree)?;
    let loaded = loaded_by_name(app);
    let mut out = Vec::new();
    for spec in specs {
        let (name, version) = package(&spec.target);
        if let Some(target) = target {
            if target != spec.target && target != name {
                continue;
            }
        }
        let loaded_plugin = loaded.get(&name).cloned();
        let runtime = loaded_plugin
            .as_ref()
            .and_then(|plugin| plugin.pointer("/features/server"))
            .and_then(Value::as_bool)
            .map(|server| if server { "Server" } else { "TUI" })
            .unwrap_or("Server");
        let mut item = Item {
            runtime,
            target: spec.target.clone(),
            name: name.clone(),
            version: version.clone().or_else(|| {
                loaded_plugin
                    .as_ref()
                    .and_then(|plugin| plugin.pointer("/source/version"))
                    .and_then(Value::as_str)
                    .map(String::from)
            }),
            latest: None,
            outdated: false,
            error: None,
            loaded: loaded_plugin,
        };
        match latest(&name).await {
            Ok(latest) => {
                if let Some(version) = &item.version {
                    item.outdated = exact(version) && version != &latest;
                }
                if item.version.is_none() {
                    item.version = Some(latest.clone());
                }
                item.latest = Some(latest);
            }
            Err(error) => item.error = Some(error.to_string()),
        }
        out.push(item);
    }
    if let Some(target) = target {
        if out.is_empty() {
            return Err(anyhow!("plugin is not configured: {target}"));
        }
    }
    Ok(out)
}

pub fn plugin_info(item: &Item) -> Value {
    let mut source = json!({
        "type": "package",
        "target": item.name,
    });
    if let Some(version) = &item.version {
        source["version"] = json!(version);
    }
    source["outdated"] = json!(item.outdated);
    if let Some(loaded) = &item.loaded {
        let mut value = loaded.clone();
        value["source"] = source;
        if let Some(error) = &item.error {
            value["state"] = json!({ "status": "failed", "error": error });
        }
        return value;
    }
    let mut value = json!({
        "id": item.name,
        "source": source,
        "features": { "server": item.runtime == "Server" },
        "state": match &item.error {
            Some(error) => json!({ "status": "failed", "error": error }),
            None => json!({ "status": "active" }),
        },
    });
    value["mode"] = json!("promise");
    value
}

fn registry_paths(app: &crate::app::App) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let config = crate::paths::get().config.clone();
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        let path = config.join(name);
        if path.is_file() {
            paths.push(path);
        }
    }
    for name in ["opencode.json", "opencode.jsonc"] {
        let path = app.worktree.join(".opencode").join(name);
        if path.is_file() && !paths.contains(&path) {
            paths.push(path);
        }
        let path = app.directory.join(".opencode").join(name);
        if path.is_file() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn rewrite_entry(entry: &mut Value, target: &str, name: &str, latest: &str) -> bool {
    let matches = entry.as_str() == Some(target)
        || entry.get("package").and_then(Value::as_str) == Some(target)
        || entry.as_str().is_some_and(|value| package(value).0 == name)
        || entry
            .get("package")
            .and_then(Value::as_str)
            .is_some_and(|value| package(value).0 == name);
    if !matches {
        return false;
    }
    let replacement = format!("{name}@{latest}");
    if entry.is_string() {
        *entry = json!(replacement);
        return true;
    }
    if let Some(object) = entry.as_object_mut() {
        object.insert("package".into(), json!(replacement));
        return true;
    }
    false
}

/// Update pinned package targets to their latest release. Returns the targets
/// that were rewritten.
pub async fn update(app: &crate::app::App, targets: &[String]) -> Result<Vec<String>> {
    let items = inspect(app, None).await?;
    let selected: Vec<&Item> = items
        .iter()
        .filter(|item| targets.is_empty() || targets.iter().any(|target| target == &item.target || target == &item.name))
        .filter(|item| item.outdated)
        .collect();
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let mut updated = Vec::new();
    for path in registry_paths(app) {
        let text = std::fs::read_to_string(&path)?;
        let mut value: Value = if path.extension().and_then(|value| value.to_str()) == Some("jsonc") {
            serde_json::from_str(&crate::config::strip_jsonc(&text))?
        } else {
            serde_json::from_str(&text)?
        };
        let mut changed = false;
        for key in ["plugins", "plugin"] {
            let Some(entries) = value.get_mut(key).and_then(Value::as_array_mut) else { continue };
            for item in &selected {
                let latest = item.latest.as_deref().unwrap_or_default();
                if latest.is_empty() {
                    continue;
                }
                for entry in entries.iter_mut() {
                    if rewrite_entry(entry, &item.target, &item.name, latest) {
                        changed = true;
                        updated.push(item.target.clone());
                    }
                }
            }
        }
        if changed {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = path.with_extension("json.tmp");
            std::fs::write(&temporary, serde_json::to_vec_pretty(&value)?)?;
            std::fs::rename(temporary, path)?;
        }
    }
    updated.sort();
    updated.dedup();
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_specs() {
        assert_eq!(package("foo@1.2.3"), ("foo".into(), Some("1.2.3".into())));
        assert_eq!(package("@scope/foo@1.2.3"), ("@scope/foo".into(), Some("1.2.3".into())));
        assert_eq!(package("@scope/foo"), ("@scope/foo".into(), None));
        assert_eq!(package("npm:foo@1.0.0"), ("foo".into(), Some("1.0.0".into())));
    }

    #[test]
    fn rewrites_pinned_entries() {
        let mut entry = json!("foo@1.0.0");
        assert!(rewrite_entry(&mut entry, "foo@1.0.0", "foo", "2.0.0"));
        assert_eq!(entry, json!("foo@2.0.0"));
        let mut object = json!({ "package": "foo@1.0.0", "options": {} });
        assert!(rewrite_entry(&mut object, "foo@1.0.0", "foo", "2.0.0"));
        assert_eq!(object["package"], "foo@2.0.0");
    }
}
