//! Loading and merging of `opencode.json` / `opencode.jsonc` configuration.
//!
//! Sources, lowest to highest precedence:
//! global config dir, `OPENCODE_CONFIG`, project files from the worktree root
//! down to the working directory (`opencode.json[c]` and `.opencode/opencode.json[c]`),
//! then `OPENCODE_CONFIG_CONTENT`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub default_agent: Option<String>,
    pub username: Option<String>,
    pub provider: BTreeMap<String, ProviderConfig>,
    pub disabled_providers: Vec<String>,
    pub enabled_providers: Option<Vec<String>>,
    pub agent: BTreeMap<String, AgentConfig>,
    pub command: BTreeMap<String, CommandConfig>,
    pub instructions: Vec<String>,
    #[serde(alias = "permissions")]
    pub permission: Option<Value>,
    #[serde(default, alias = "policy")]
    pub policies: Vec<crate::policy::Policy>,
    pub mcp: BTreeMap<String, McpConfig>,
    pub tools: BTreeMap<String, bool>,
    pub experimental: Experimental,
    pub compaction: Compaction,
    pub skills: Vec<String>,
    pub websearch: Option<WebSearchConfig>,
    pub formatter: Option<Value>,
    pub lsp: Option<Value>,
    pub media: MediaConfig,
    pub reference: BTreeMap<String, Value>,
    pub shell: Option<Value>,
    pub tool_output: ToolOutputConfig,
    pub warming: Option<Value>,
    pub watcher: WatcherConfig,
    pub worktree: Option<WorktreeConfig>,
    pub debug: DebugConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Experimental {
    pub todo: bool,
    pub schedules: bool,
    #[serde(alias = "codemodeVm", alias = "codemode_vm")]
    pub codemode: bool,
    pub background_compaction: bool,
    pub media_blob: bool,
    pub artifacts: bool,
    pub console_watches: bool,
    pub all_flags: bool,
    pub part_rows: bool,
    pub image_generate: bool,
    pub subagent_depth: Option<u64>,
    pub location_idle_timeout: Option<u64>,
    #[serde(rename = "new-designs")]
    pub new_designs: bool,
    #[serde(rename = "client-mcp")]
    pub client_mcp: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub image: MediaImageConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaImageConfig {
    pub auto_resize: Option<bool>,
    pub max_width: Option<u64>,
    pub max_height: Option<u64>,
    pub max_base64_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    pub max_lines: Option<u64>,
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WatcherConfig {
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    pub devtools: bool,
    pub timing: bool,
    pub turn_tokens: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Keep {
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub auto: bool,
    pub keep: Option<Keep>,
    /// A percentage such as "20%" or a token count kept free below the window.
    pub buffer: Option<Value>,
}
impl Default for Compaction {
    fn default() -> Self {
        Compaction { auto: true, keep: None, buffer: None }
    }
}

#[cfg(test)]
mod compaction_tests {
    use super::*;

    #[test]
    fn buffer_accepts_percent_and_tokens() {
        let mut compaction = Compaction::default();
        assert!((compaction.threshold(100_000) - 0.85).abs() < f64::EPSILON);
        compaction.buffer = Some(serde_json::json!("20%"));
        assert!((compaction.threshold(100_000) - 0.8).abs() < f64::EPSILON);
        compaction.buffer = Some(serde_json::json!(30_000));
        assert!((compaction.threshold(100_000) - 0.7).abs() < f64::EPSILON);
    }
}

impl Compaction {
    pub fn threshold(&self, context: u64) -> f64 {
        if context == 0 {
            return 0.85;
        }
        match &self.buffer {
            Some(Value::String(value)) => value
                .strip_suffix('%')
                .and_then(|value| value.parse::<f64>().ok())
                .map(|percent| (1.0 - percent / 100.0).clamp(0.01, 0.99))
                .unwrap_or(0.85),
            Some(Value::Number(value)) => value
                .as_u64()
                .map(|tokens| ((context.saturating_sub(tokens)) as f64 / context as f64).clamp(0.01, 0.99))
                .unwrap_or(0.85),
            _ => 0.85,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub env: Vec<String>,
    pub options: ProviderOptions,
    pub models: BTreeMap<String, Value>,
    pub whitelist: Option<Vec<String>>,
    pub blacklist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderOptions {
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "baseURL")]
    pub base_url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub timeout: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub model: Option<String>,
    pub variant: Option<String>,
    #[serde(alias = "system")]
    pub prompt: Option<String>,
    pub description: Option<String>,
    pub mode: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub steps: Option<usize>,
    #[serde(alias = "maxSteps")]
    pub max_steps: Option<usize>,
    pub tools: BTreeMap<String, bool>,
    #[serde(alias = "permissions")]
    pub permission: Option<Value>,
    #[serde(alias = "disabled")]
    pub disable: bool,
    pub hidden: bool,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandConfig {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    #[serde(alias = "subagent")]
    pub subtask: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpConfig {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub codemode: Option<bool>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub oauth: Option<serde_json::Value>,
    pub protocol: Option<String>,
    pub pool: Option<bool>,
    pub enabled: Option<bool>,
    /// A legacy millisecond count or an object with startup/catalog/execution.
    pub timeout: Option<serde_json::Value>,
}

/// Where configuration was found, plus directories that may contain
/// `agent/`, `command/` and `skill/` folders.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    /// Configured and auto-discovered plugins, in load order.
    pub plugins: Vec<crate::plugin::PluginSpec>,
}

/// Strip `//` and `/* */` comments and trailing commas from JSONC text.
pub fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // trailing commas
    let re = regex::Regex::new(r",(\s*[}\]])").unwrap();
    re.replace_all(&out, "$1").into_owned()
}

/// Replace `{env:VAR}` and `{file:path}` tokens.
fn substitute(text: &str, dir: &Path) -> String {
    let env_re = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let text = env_re.replace_all(text, |c: &regex::Captures| std::env::var(&c[1]).unwrap_or_default());
    let file_re = regex::Regex::new(r"\{file:([^}]+)\}").unwrap();
    file_re
        .replace_all(&text, |c: &regex::Captures| {
            let p = crate::util::resolve(dir, c[1].trim());
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            // embed as JSON string body
            let s = serde_json::to_string(content.trim_end()).unwrap_or_default();
            s[1..s.len() - 1].to_string()
        })
        .into_owned()
}

pub fn parse_file(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let text = substitute(crate::util::strip_bom(&text).0, dir);
    let v: Value = serde_json::from_str(&strip_jsonc(&text)).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Deep merge `b` into `a`. Arrays of `instructions` are concatenated, others replaced.
pub fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, v) in bm {
                if matches!(k.as_str(), "instructions" | "plugins" | "plugin" | "skills") {
                    if let (Some(Value::Array(existing)), Value::Array(new)) = (am.get_mut(&k), &v) {
                        for item in new {
                            if !existing.contains(item) {
                                existing.push(item.clone());
                            }
                        }
                        continue;
                    }
                }
                match am.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        am.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Find the git worktree root by walking up for a `.git` entry.
pub fn find_worktree(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Platform configuration directory, matching upstream discovery.
pub fn system_config_dir() -> PathBuf {
    match std::env::consts::OS {
        "windows" => std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("opencode"),
        "macos" => PathBuf::from("/Library/Application Support/opencode"),
        _ => PathBuf::from("/etc/opencode"),
    }
}

fn model_selection(value: &Value) -> Option<String> {
    if let Some(short) = value.as_str() {
        return Some(short.to_string());
    }
    let object = value.as_object()?;
    let provider = object.get("providerID").and_then(Value::as_str)?;
    let model = object.get("model").or_else(|| object.get("id")).and_then(Value::as_str)?;
    let variant = object.get("variant").and_then(Value::as_str);
    Some(match variant {
        Some(variant) => format!("{provider}/{model}#{variant}"),
        None => format!("{provider}/{model}"),
    })
}

fn normalize_v2(raw: &mut Value) {
    let Some(m) = raw.as_object_mut() else { return };
    for (plural, singular) in [
        ("providers", "provider"),
        ("agents", "agent"),
        ("commands", "command"),
        ("permissions", "permission"),
    ] {
        if let Some(value) = m.remove(plural) {
            let entry = m
                .entry(singular.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            merge(entry, value);
        }
    }
    if matches!(m.get("skills"), Some(Value::Object(_))) {
        let Some(Value::Object(skills)) = m.remove("skills") else {
            return;
        };
        let mut items: Vec<Value> = Vec::new();
        for key in ["paths", "urls"] {
            if let Some(Value::Array(values)) = skills.get(key) {
                for value in values {
                    if value.as_str().is_some() && !items.contains(value) {
                        items.push(value.clone());
                    }
                }
            }
        }
        m.insert("skills".into(), Value::Array(items));
    }
    for key in ["model", "small_model"] {
        if let Some(value) = m.get_mut(key) {
            if let Some(selection) = model_selection(value) {
                *value = Value::String(selection);
            }
        }
    }
    for key in ["agent", "command"] {
        if let Some(Value::Object(entries)) = m.get_mut(key) {
            for entry in entries.values_mut() {
                if let Value::Object(fields) = entry {
                    if let Some(value) = fields.get_mut("model") {
                        if let Some(selection) = model_selection(value) {
                            *value = Value::String(selection);
                        }
                    }
                }
            }
        }
    }
    if let Some(Value::Object(mcp)) = m.get_mut("mcp") {
        let timeout = mcp.remove("timeout");
        if let Some(Value::Object(servers)) = mcp.remove("servers") {
            for (name, mut server) in servers {
                if let Value::Object(s) = &mut server {
                    if let Some(Value::Bool(disabled)) = s.remove("disabled") {
                        s.entry("enabled").or_insert(Value::Bool(!disabled));
                    }
                    if s.get("timeout").is_none() {
                        if let Some(timeout) = &timeout {
                            s.insert("timeout".into(), timeout.clone());
                        }
                    }
                }
                mcp.insert(name, server);
            }
        }
        for server in mcp.values_mut() {
            if let Value::Object(s) = server {
                if let Some(Value::Bool(disabled)) = s.remove("disabled") {
                    s.entry("enabled").or_insert(Value::Bool(!disabled));
                }
                if s.get("timeout").is_none() {
                    if let Some(timeout) = &timeout {
                        s.insert("timeout".into(), timeout.clone());
                    }
                }
            }
        }
    }
}

/// Merge a patch into the global configuration file, the same file the
/// experimental config endpoint writes.
pub fn patch_global(patch: &Value) -> Result<()> {
    let file = crate::paths::get().config.join("opencode.json");
    let mut document = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(target) = document.as_object_mut() else {
        return Err(anyhow!("configuration must be an object"));
    };
    let Some(patch) = patch.as_object() else {
        return Err(anyhow!("configuration patch must be an object"));
    };
    for (key, value) in patch {
        target.insert(key.clone(), value.clone());
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, serde_json::to_vec_pretty(&document)?)?;
    Ok(())
}

/// Set one experimental flag in the global configuration file.
pub fn set_experiment(id: &str, enabled: bool) -> Result<()> {
    let file = crate::paths::get().config.join("opencode.json");
    let mut document = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(root) = document.as_object_mut() else {
        return Err(anyhow!("configuration must be an object"));
    };
    let experimental = root.entry("experimental").or_insert_with(|| Value::Object(Map::new()));
    if !experimental.is_object() {
        *experimental = Value::Object(Map::new());
    }
    experimental.as_object_mut().unwrap().insert(id.to_string(), Value::Bool(enabled));
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, serde_json::to_vec_pretty(&document)?)?;
    Ok(())
}

pub fn load(directory: &Path, worktree: &Path) -> Result<Loaded> {
    let paths = crate::paths::get();
    let mut raw = Value::Object(Map::new());
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let system = system_config_dir();
    if system.is_dir() {
        directories.push(system.clone());
    }
    // Integration sources are the lowest config layer; local and project
    // configuration is merged over them. Their policies are applied last.
    let mut source_policies = Vec::new();
    for source in crate::wellknown::configs().into_iter().chain(crate::console::configs()) {
        if let Some(policies) = source.get("policies").and_then(Value::as_array) {
            source_policies.extend(policies.iter().cloned());
        }
        merge(&mut raw, source);
    }

    let add = |raw: &mut Value, files: &mut Vec<PathBuf>, p: PathBuf| -> Result<()> {
        if p.is_file() {
            let mut v = parse_file(&p)?;
            if let Some(parent) = p.parent() {
                crate::plugin::absolutize_config(&mut v, parent);
            }
            merge(raw, v);
            files.push(p);
        }
        Ok(())
    };

    if system.is_dir() {
        for name in ["opencode.json", "opencode.jsonc"] {
            add(&mut raw, &mut files, system.join(name))?;
        }
    }
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        add(&mut raw, &mut files, paths.config.join(name))?;
    }
    directories.push(paths.config.clone());
    if let Ok(p) = std::env::var("OPENCODE_CONFIG") {
        add(&mut raw, &mut files, PathBuf::from(p))?;
    }

    // project: from worktree root down to the directory
    let mut chain = Vec::new();
    let mut cur = Some(directory);
    while let Some(dir) = cur {
        chain.push(dir.to_path_buf());
        if crate::util::normalize(dir) == crate::util::normalize(worktree) {
            break;
        }
        cur = dir.parent();
    }
    chain.reverse();
    for dir in &chain {
        for name in ["opencode.json", "opencode.jsonc"] {
            add(&mut raw, &mut files, dir.join(name))?;
        }
        let local = dir.join(".opencode");
        if local.is_dir() {
            directories.push(local.clone());
            for file in ["opencode.json", "opencode.jsonc"] {
                add(&mut raw, &mut files, local.join(file))?;
            }
        }
    }
    if let Ok(content) = std::env::var("OPENCODE_CONFIG_CONTENT") {
        let mut v: Value = serde_json::from_str(&strip_jsonc(&substitute(&content, directory)))
            .context("parsing OPENCODE_CONFIG_CONTENT")?;
        crate::plugin::absolutize_config(&mut v, directory);
        merge(&mut raw, v);
    }
    if !source_policies.is_empty() {
        let policies = raw
            .as_object_mut()
            .expect("configuration is an object")
            .entry("policies")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(existing) = policies {
            existing.extend(source_policies);
        }
    }

    normalize_v2(&mut raw);
    if let Value::Object(m) = &mut raw {
        if let Some(mode) = m.remove("mode") {
            let agents = m.entry("agent").or_insert_with(|| Value::Object(Map::new()));
            let mut mode = mode;
            if let Value::Object(mm) = &mut mode {
                for v in mm.values_mut() {
                    if let Value::Object(o) = v {
                        o.entry("mode").or_insert(Value::String("primary".into()));
                    }
                }
            }
            merge(agents, mode);
        }
    }

    let plugins = crate::plugin::collect_specs(&raw, &directories);
    let config: Config = serde_json::from_value(raw.clone()).context("invalid configuration")?;
    Ok(Loaded { config, files, directories, plugins })
}

/// Parse `---` YAML-ish frontmatter (flat `key: value` pairs, with simple
/// nested maps one level deep) from markdown files used by agents, commands and skills.
pub fn frontmatter(text: &str) -> (Map<String, Value>, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut map = Map::new();
    let Some(rest) = text.strip_prefix("---") else {
        return (map, text.to_string());
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else {
        return (map, text.to_string());
    };
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n', '-']).to_string();
    let mut current: Option<String> = None;
    for line in header.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        let key = k.trim().trim_matches('"').to_string();
        let val = v.trim();
        if indented {
            if let Some(parent) = &current {
                let entry = map.entry(parent.clone()).or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(o) = entry {
                    o.insert(key, yaml_scalar(val));
                }
            }
            continue;
        }
        if val.is_empty() {
            current = Some(key.clone());
            map.insert(key, Value::Object(Map::new()));
        } else {
            current = None;
            map.insert(key, yaml_scalar(val));
        }
    }
    (map, body)
}

fn yaml_scalar(v: &str) -> Value {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" => return Value::Null,
        _ => {}
    }
    if let Ok(n) = v.parse::<i64>() {
        return Value::from(n);
    }
    if let Ok(f) = v.parse::<f64>() {
        return Value::from(f);
    }
    if v.starts_with('[') && v.ends_with(']') {
        let items = v[1..v.len() - 1]
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(yaml_scalar)
            .collect();
        return Value::Array(items);
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonc() {
        let t = "{ // c\n \"a\": \"http://x\", /* b */ \"b\": [1,2,],\n}";
        let v: Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["a"], "http://x");
        assert_eq!(v["b"][1], 2);
    }
    #[test]
    fn accepts_v2_agent_fields() {
        let agent: AgentConfig = serde_json::from_value(serde_json::json!({
            "system": "be brief",
            "disabled": true,
            "permissions": [{ "action": "edit", "resource": "*", "effect": "deny" }]
        }))
        .unwrap();
        assert_eq!(agent.prompt.as_deref(), Some("be brief"));
        assert!(agent.disable);
        assert!(agent.permission.is_some());
    }

    #[test]
    fn normalizes_v2_keys() {
        let mut raw = serde_json::json!({
            "model": { "providerID": "p", "model": "m", "variant": "fast" },
            "providers": { "p": { "name": "P" } },
            "agents": { "a": { "mode": "primary", "model": { "providerID": "p", "model": "m" } } },
            "commands": { "c": { "template": "hi", "model": { "providerID": "p", "model": "m" } } },
            "permissions": { "edit": "allow" },
            "skills": { "paths": ["~/.claude/skills"], "urls": ["https://example.com/skills"] },
            "mcp": {
                "timeout": { "startup": 1000 },
                "servers": {
                    "s": { "type": "local", "command": ["x"], "disabled": true }
                }
            }
        });
        normalize_v2(&mut raw);
        assert!(raw.get("provider").is_some());
        assert!(raw.get("agent").is_some());
        assert!(raw.get("command").is_some());
        assert!(raw.get("permission").is_some());
        assert_eq!(raw["skills"], serde_json::json!(["~/.claude/skills", "https://example.com/skills"]));
        assert_eq!(raw["model"], "p/m#fast");
        assert_eq!(raw["agent"]["a"]["model"], "p/m");
        assert_eq!(raw["command"]["c"]["model"], "p/m");
        assert_eq!(raw.pointer("/mcp/s/enabled"), Some(&serde_json::json!(false)));
        assert_eq!(raw.pointer("/mcp/s/timeout/startup"), Some(&serde_json::json!(1000)));
        assert!(raw.pointer("/mcp/timeout").is_none());
    }

    #[test]
    fn merges_plugin_lists() {
        let mut a = serde_json::json!({ "plugins": ["global"], "plugin": ["old"] });
        merge(&mut a, serde_json::json!({ "plugins": ["project"], "plugin": ["new"] }));
        assert_eq!(a["plugins"], serde_json::json!(["global", "project"]));
        assert_eq!(a["plugin"], serde_json::json!(["old", "new"]));
    }

    #[test]
    fn parses_extended_groups() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "media": { "image": { "auto_resize": true, "max_width": 1024 } },
            "reference": { "docs": { "repository": "owner/repo" } },
            "shell": { "path": "/bin/zsh" },
            "tool_output": { "max_lines": 500, "max_bytes": 20000 },
            "warming": { "prompt": "hi", "interval": "auto" },
            "watcher": { "ignore": ["dist"] },
            "worktree": { "directory": "../worktrees" },
            "experimental": {
                "background_compaction": true,
                "media_blob": true,
                "artifacts": true,
                "console_watches": true,
                "part_rows": true,
                "image_generate": true,
                "subagent_depth": 3,
                "location_idle_timeout": 60
            }
        }))
        .unwrap();
        assert_eq!(config.media.image.max_width, Some(1024));
        assert!(config.reference.contains_key("docs"));
        assert!(config.shell.is_some());
        assert_eq!(config.tool_output.max_lines, Some(500));
        assert_eq!(config.watcher.ignore, vec!["dist"]);
        assert_eq!(config.worktree.unwrap().directory.as_deref(), Some("../worktrees"));
        assert!(config.experimental.background_compaction);
        assert_eq!(config.experimental.subagent_depth, Some(3));
    }

    #[test]
    fn fm() {
        let (m, body) = frontmatter("---\nname: x\ndescription: \"hello: world\"\ntools:\n  write: false\n---\nBody");
        assert_eq!(m["name"], "x");
        assert_eq!(m["description"], "hello: world");
        assert_eq!(m["tools"]["write"], false);
        assert_eq!(body, "Body");
    }
}
