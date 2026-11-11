//! Open session tabs, persisted per working directory in upstream's `tui/tabs.json` shape
//! (`{ global, cwd: { <dir>: { tabs: [{ sessionID, title }], unread } } }`).
//!
//! The file lives apart from upstream's own (`<state>/latest/tui/tabs.json`): this client keeps
//! sessions in a different store, so its session ids would be dangling tabs there.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Closed tabs kept for reopening (upstream `CLOSED_SESSION_TAB_LIMIT`).
pub const CLOSED_LIMIT: usize = 25;

fn file() -> PathBuf {
    crate::paths::get().state.join("rs").join("tui").join("tabs.json")
}

fn read() -> Value {
    std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({ "global": { "tabs": [], "unread": {} }, "cwd": {} }))
}

fn key(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

pub fn load(dir: &Path) -> Vec<String> {
    read()
        .pointer("/cwd")
        .and_then(|c| c.get(key(dir)))
        .and_then(|s| s.get("tabs"))
        .and_then(Value::as_array)
        .map(|tabs| tabs.iter().filter_map(|t| t.get("sessionID").and_then(Value::as_str).map(String::from)).collect())
        .unwrap_or_default()
}

/// Write `tabs` as (session id, title) for `dir`, keeping other directories' entries.
pub fn save(dir: &Path, tabs: &[(String, String)]) {
    let mut doc = read();
    let list: Vec<Value> = tabs.iter().map(|(id, title)| json!({ "sessionID": id, "title": title })).collect();
    if !doc.get("cwd").is_some_and(Value::is_object) {
        doc["cwd"] = json!({});
    }
    doc["cwd"][key(dir)] = json!({ "tabs": list, "unread": {} });
    let path = file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, doc.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
