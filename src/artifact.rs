//! Local artifact storage and the built-in artifact tools.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    crate::paths::get().storage().join("artifacts")
}

fn dir(id: &str) -> PathBuf {
    root().join(id)
}

fn entry(files: &Map<String, Value>, requested: Option<&str>) -> Result<String> {
    if let Some(entry) = requested.filter(|entry| !entry.is_empty()) {
        if files.contains_key(entry) {
            return Ok(entry.to_string());
        }
        return Err(anyhow!("artifact entry not found: {entry}"));
    }
    for candidate in ["index.html", "index.md"] {
        if files.contains_key(candidate) {
            return Ok(candidate.to_string());
        }
    }
    if files.len() == 1 {
        if let Some(file) = files.keys().next() {
            return Ok(file.clone());
        }
    }
    Err(anyhow!("artifact needs an entry file"))
}

fn info(id: &str, meta: &Value) -> Value {
    let mut value = meta.clone();
    value["id"] = json!(id);
    let entry = meta.get("entry").and_then(Value::as_str).unwrap_or("index.html");
    value["url"] = json!(url::Url::from_file_path(dir(id).join(entry))
        .map(|url| url.to_string())
        .unwrap_or_default());
    value
}

fn write_files(id: &str, files: &Map<String, Value>) -> Result<()> {
    let root = dir(id);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    for (name, value) in files {
        let Some(text) = value.as_str() else { continue };
        let path = root.join(name);
        if path.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
            return Err(anyhow!("artifact path escapes its root: {name}"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    }
    Ok(())
}

pub fn create(input: &Value) -> Result<Value> {
    let title = input.get("title").and_then(Value::as_str).unwrap_or("Artifact").to_string();
    let files = input
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("artifact_create requires files"))?;
    let entry = entry(files, input.get("entry").and_then(Value::as_str))?;
    let id = format!("art_{}", crate::util::id("").trim_start_matches('_'));
    let now = crate::util::now_ms();
    let meta = json!({
        "provider": input.get("provider").and_then(Value::as_str).unwrap_or("local"),
        "sessionID": input.get("sessionID").cloned(),
        "title": title,
        "entry": entry,
        "time": { "created": now, "updated": now },
    });
    write_files(&id, files)?;
    std::fs::create_dir_all(dir(&id))?;
    std::fs::write(dir(&id).join("meta.json"), serde_json::to_vec(&meta)?)?;
    Ok(info(&id, &meta))
}

fn load(id: &str) -> Result<Value> {
    let text = std::fs::read_to_string(dir(id).join("meta.json"))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn get(id: &str) -> Result<Value> {
    let meta = load(id)?;
    let mut files = Map::new();
    let root = dir(id);
    for item in walk(&root)? {
        if item.ends_with("meta.json") {
            continue;
        }
        let relative = item.strip_prefix(&root).unwrap_or(&item).to_string_lossy().replace('\\', "/");
        if let Ok(text) = std::fs::read_to_string(&item) {
            files.insert(relative, json!(text));
        }
    }
    Ok(json!({ "info": info(id, &meta), "files": files }))
}

pub fn update(id: &str, input: &Value) -> Result<Value> {
    let mut meta = load(id)?;
    let files = input
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("artifact_update requires files"))?;
    let entry = entry(files, input.get("entry").and_then(Value::as_str).or_else(|| meta.get("entry").and_then(Value::as_str)))?;
    let now = crate::util::now_ms();
    if let Some(title) = input.get("title").and_then(Value::as_str) {
        meta["title"] = json!(title);
    }
    meta["entry"] = json!(entry);
    meta["time"]["updated"] = json!(now);
    write_files(id, files)?;
    std::fs::write(dir(id).join("meta.json"), serde_json::to_vec(&meta)?)?;
    Ok(info(id, &meta))
}

pub fn delete(id: &str) -> Result<bool> {
    if !dir(id).is_dir() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir(id))?;
    Ok(true)
}

pub fn list() -> Result<Vec<Value>> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root()) {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Ok(meta) = load(&id) {
                out.push(info(&id, &meta));
            }
        }
    }
    out.sort_by_key(|value| value.pointer("/time/created").and_then(Value::as_i64).unwrap_or(0));
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_read_update_delete() {
        let info = create(&json!({
            "title": "Report",
            "files": { "index.html": "<h1>hi</h1>" },
        }))
        .unwrap();
        let id = info["id"].as_str().unwrap();
        assert!(info["url"].as_str().unwrap().contains("index.html"));
        let content = get(id).unwrap();
        assert_eq!(content["files"]["index.html"], "<h1>hi</h1>");
        let updated = update(id, &json!({
            "title": "Report 2",
            "files": { "index.html": "<h1>bye</h1>" },
        }))
        .unwrap();
        assert_eq!(updated["title"], "Report 2");
        assert!(delete(id).unwrap());
        assert!(get(id).is_err());
    }
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(current)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}
