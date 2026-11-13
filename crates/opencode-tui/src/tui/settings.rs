//! Client settings shared with the TypeScript TUI: `<config>/cli.json` (v2) with
//! `tui.json` as a fallback. Toggles are written back so both clients agree.

use serde_json::{json, Value};
use std::path::PathBuf;

pub struct Settings {
    path: PathBuf,
    pub data: Value,
}

fn read(p: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str::<Value>(&crate::config::strip_jsonc(&text)).ok().filter(|v| v.is_object())
}

impl Settings {
    pub fn load() -> Settings {
        let dir = &crate::paths::get().config;
        let path = dir.join("cli.json");
        let data = read(&path).or_else(|| read(&dir.join("tui.json"))).unwrap_or_else(|| json!({}));
        Settings { path, data }
    }

    fn get(&self, path: &str) -> Option<&Value> {
        path.split('.').try_fold(&self.data, |v, k| v.get(k))
    }

    pub fn str(&self, path: &str) -> Option<&str> {
        self.get(path).and_then(|v| v.as_str())
    }

    pub fn bool(&self, path: &str) -> Option<bool> {
        self.get(path).and_then(|v| v.as_bool())
    }

    pub fn theme(&self) -> String {
        self.str("theme.name").unwrap_or("opencode").to_string()
    }

    /// "show" or "hide"; upstream defaults to hide.
    pub fn thinking_shown(&self) -> bool {
        self.str("session.thinking") == Some("show")
    }

    /// "auto" or "hide".
    pub fn sidebar_auto(&self) -> bool {
        self.str("session.sidebar").unwrap_or("auto") == "auto"
    }

    pub fn grouping(&self) -> bool {
        self.str("session.grouping") != Some("none")
    }

    pub fn tps(&self) -> bool {
        self.bool("session.tps").unwrap_or(true)
    }

    pub fn animations(&self) -> bool {
        self.bool("animations").unwrap_or(true)
    }

    pub fn mouse(&self) -> bool {
        self.bool("mouse").unwrap_or(true)
    }

    pub fn keybinds(&self) -> Value {
        self.get("keybinds").cloned().unwrap_or_else(|| json!({}))
    }

    /// Set `path` (dotted) and persist.
    pub fn set(&mut self, path: &str, value: Value) {
        let keys: Vec<&str> = path.split('.').collect();
        let mut cur = &mut self.data;
        for k in &keys[..keys.len() - 1] {
            if !cur.get(*k).is_some_and(|v| v.is_object()) {
                cur[*k] = json!({});
            }
            cur = &mut cur[*k];
        }
        cur[keys[keys.len() - 1]] = value;
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let mut doc = self.data.clone();
        if doc.get("$schema").is_none() {
            if let Some(o) = doc.as_object_mut() {
                o.insert("$schema".into(), json!("https://opencode.ai/v2/cli.json"));
            }
        }
        let _ = std::fs::write(&self.path, serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n");
    }
}
