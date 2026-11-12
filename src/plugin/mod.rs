//! OpenCode V2 plugins.
//!
//! Plugins are TypeScript/JavaScript modules executed by a Deno sidecar. The
//! host exposes the V2 Promise API (`export default { id, setup }`) and Effect
//! API (`export default { id, effect }`), plus the legacy V1 plugin export
//! shape. Communication is newline-delimited JSON-RPC over stdin/stdout.
//!
//! The Deno binary is discovered from `OPENCODE_DENO`, `PATH`, or the opencode
//! cache. When it is missing, a pinned official release is downloaded on first
//! use.

pub(crate) mod pty;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const DENO_VERSION: &str = "2.9.8";

/// Cancellation tokens for in-flight `client.call` requests, keyed by the
/// request id allocated by the Deno host when a plugin passes an AbortSignal.
static CLIENT_CANCELS: std::sync::OnceLock<Mutex<HashMap<String, CancellationToken>>> =
    std::sync::OnceLock::new();

fn client_cancels() -> &'static Mutex<HashMap<String, CancellationToken>> {
    CLIENT_CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}
const HOST_TS: &str = include_str!("host.ts");
const PLUGIN_API_VERSION: &str = "2.2.7";
const V1_PLUGIN_API_VERSION: &str = "1.19.5";

pub fn kill_terminal(session_id: &str) {
    pty::manager().kill(session_id);
}
const EFFECT_VERSION: &str = "4.1.0";

// ---------------------------------------------------------------- plugin specs

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub target: String,
    pub options: Value,
}

impl PluginSpec {
    fn identity(&self) -> String {
        if is_local(&self.target) {
            return normalize_path(&self.target);
        }
        parse_package(&self.target).0
    }
}

fn is_local(target: &str) -> bool {
    target.starts_with("file://")
        || target.starts_with("./")
        || target.starts_with("../")
        || target.starts_with(".\\")
        || target.starts_with("..\\")
        || Path::new(target).is_absolute()
        || (target.len() > 2 && target.as_bytes()[1] == b':' && matches!(target.as_bytes()[2], b'\\' | b'/'))
}

fn local_path(target: &str) -> PathBuf {
    if target.starts_with("file://") {
        if let Ok(url) = url::Url::parse(target) {
            if let Ok(path) = url.to_file_path() {
                return path;
            }
        }
        return PathBuf::from(target.trim_start_matches("file://"));
    }
    PathBuf::from(target)
}

fn normalize_path(target: &str) -> String {
    std::fs::canonicalize(local_path(target))
        .unwrap_or_else(|_| local_path(target))
        .to_string_lossy()
        .replace('\\', "/")
}

/// Split `pkg@1.2.3` / `@scope/pkg@1.2.3` into (name, version).
fn parse_package(spec: &str) -> (String, Option<String>) {
    let rest = spec.strip_prefix("npm:").unwrap_or(spec);
    if let Some(at) = rest.rfind('@') {
        if at > 0 && !rest[at - 1..].starts_with('@') {
            return (rest[..at].to_string(), Some(rest[at + 1..].to_string()));
        }
    }
    (rest.to_string(), None)
}

fn resolve_local_target(target: &str, base: &Path) -> String {
    if target.starts_with("file://") {
        return target.to_string();
    }
    let raw = Path::new(target);
    if raw.is_absolute() || (target.len() > 2 && target.as_bytes()[1] == b':' && matches!(target.as_bytes()[2], b'\\' | b'/')) {
        return crate::util::normalize(raw).to_string_lossy().to_string();
    }
    crate::util::normalize(&base.join(raw)).to_string_lossy().to_string()
}

/// Turn relative plugin paths into absolute paths while the config file that
/// declared them is still known.
pub fn absolutize_config(value: &mut Value, dir: &Path) {
    let Some(obj) = value.as_object_mut() else { return };
    if let Some(Value::Array(items)) = obj.get_mut("plugins") {
        for item in items {
            match item {
                Value::String(s) => {
                    let (remove, target) = match s.strip_prefix('-') {
                        Some(rest) => (true, rest),
                        None => (false, s.as_str()),
                    };
                    if is_local(target) {
                        let resolved = resolve_local_target(target, dir);
                        *s = if remove { format!("-{resolved}") } else { resolved };
                    }
                }
                Value::Object(o) => {
                    if let Some(Value::String(s)) = o.get_mut("package") {
                        let (remove, target) = match s.strip_prefix('-') {
                            Some(rest) => (true, rest.to_string()),
                            None => (false, s.clone()),
                        };
                        if is_local(&target) {
                            let resolved = resolve_local_target(&target, dir);
                            *s = if remove { format!("-{resolved}") } else { resolved };
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(Value::Array(items)) = obj.get_mut("plugin") {
        for item in items {
            match item {
                Value::String(s) => {
                    let (remove, target) = match s.strip_prefix('-') {
                        Some(rest) => (true, rest),
                        None => (false, s.as_str()),
                    };
                    if is_local(target) {
                        let resolved = resolve_local_target(target, dir);
                        *s = if remove { format!("-{resolved}") } else { resolved };
                    }
                }
                Value::Array(tuple) => {
                    if let Some(Value::String(s)) = tuple.get_mut(0) {
                        let (remove, target) = match s.strip_prefix('-') {
                            Some(rest) => (true, rest.to_string()),
                            None => (false, s.clone()),
                        };
                        if is_local(&target) {
                            let resolved = resolve_local_target(&target, dir);
                            *s = if remove { format!("-{resolved}") } else { resolved };
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn push_spec(out: &mut Vec<PluginSpec>, target: String, options: Value) {
    out.push(PluginSpec { target, options });
}

/// Collect configured and auto-discovered plugins. `plugins` is the V2 key,
/// `plugin` the legacy key. Auto-discovered files come first and configured
/// entries last, so configuration can disable a discovered plugin.
pub fn collect_specs(config: &Value, directories: &[PathBuf]) -> Vec<PluginSpec> {
    let mut list = Vec::new();
    for dir in directories {
        for sub in ["plugin", "plugins"] {
            let Ok(read) = std::fs::read_dir(dir.join(sub)) else { continue };
            let mut entries: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                let ok = path.is_dir()
                    || path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| matches!(ext, "ts" | "js" | "mjs" | "cjs" | "tsx" | "jsx"));
                if ok {
                    push_spec(&mut list, path.to_string_lossy().to_string(), json!({}));
                }
            }
        }
    }
    let add = |list: &mut Vec<PluginSpec>, value: &Value| match value {
        Value::String(s) if s.starts_with('-') => {
            let id = PluginSpec { target: s[1..].to_string(), options: Value::Null }.identity();
            list.retain(|spec| spec.identity() != id);
        }
        Value::String(s) => push_spec(list, s.clone(), json!({})),
        Value::Array(tuple) if !tuple.is_empty() => {
            let target = tuple[0].as_str().unwrap_or_default().to_string();
            if target.starts_with('-') {
                let id = PluginSpec { target: target[1..].to_string(), options: Value::Null }.identity();
                list.retain(|spec| spec.identity() != id);
            } else if !target.is_empty() {
                push_spec(list, target, tuple.get(1).cloned().unwrap_or(json!({})));
            }
        }
        Value::Object(o) => {
            if let Some(Value::String(target)) = o.get("package") {
                if target.starts_with('-') {
                    let id = PluginSpec { target: target[1..].to_string(), options: Value::Null }.identity();
                    list.retain(|spec| spec.identity() != id);
                } else if !target.is_empty() {
                    push_spec(list, target.clone(), o.get("options").cloned().unwrap_or(json!({})));
                }
            }
        }
        _ => {}
    };
    if let Some(Value::Array(items)) = config.get("plugins") {
        for item in items {
            add(&mut list, item);
        }
    }
    if let Some(Value::Array(items)) = config.get("plugin") {
        for item in items {
            add(&mut list, item);
        }
    }
    // Later declarations win. Keep the last occurrence for each identity.
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for spec in list.into_iter().rev() {
        if seen.insert(spec.identity()) {
            deduped.push(spec);
        }
    }
    deduped.reverse();
    deduped
}

fn entry_candidates(spec: &PluginSpec) -> Result<Vec<String>> {
    if is_local(&spec.target) {
        let path = local_path(&spec.target);
        if path.is_file() {
            return Ok(vec![file_url(&path)]);
        }
        if path.is_dir() {
            let entry = local_directory_entry(&path)
                .ok_or_else(|| anyhow!("plugin directory {} has no server entry (package.json exports, main, or index.ts/js)", path.display()))?;
            return Ok(vec![file_url(&entry)]);
        }
        return Err(anyhow!("plugin path does not exist: {}", path.display()));
    }
    let (name, version) = parse_package(&spec.target);
    let at = match version {
        Some(v) => format!("npm:{name}@{v}"),
        None => format!("npm:{name}@latest"),
    };
    // V2 packages normally export a root or `./server` entry. Try both, plus
    // the conventional dist path used by some older packages.
    Ok(vec![
        at.clone(),
        format!("{at}/server"),
        format!("{at}/dist/server.js"),
    ])
}

/// TUI entry candidates for a plugin, or none when it has no TUI half.
fn tui_entry_candidates(spec: &PluginSpec) -> Vec<String> {
    if is_local(&spec.target) {
        let path = local_path(&spec.target);
        if path.is_file() {
            return Vec::new();
        }
        let Some(dir) = path.is_dir().then_some(path) else { return Vec::new() };
        if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(manifest) = serde_json::from_str::<Value>(crate::util::strip_bom(&text).0) {
                if let Some(exports) = manifest.get("exports") {
                    if let Some(entry) = exports.get("./tui") {
                        let raw = match entry {
                            Value::String(value) => Some(value.clone()),
                            Value::Object(map) => map
                                .get("import")
                                .or_else(|| map.get("default"))
                                .and_then(Value::as_str)
                                .map(String::from),
                            _ => None,
                        };
                        if let Some(raw) = raw {
                            let raw = raw.strip_prefix("file://").unwrap_or(&raw);
                            let file = if Path::new(raw).is_absolute() {
                                PathBuf::from(raw)
                            } else {
                                dir.join(raw)
                            };
                            let file = crate::util::normalize(&file);
                            if file.is_file() {
                                return vec![file_url(&file)];
                            }
                        }
                    }
                }
            }
        }
        for name in ["tui.tsx", "tui.ts", "tui.js", "tui.mjs", "tui.cjs"] {
            let file = dir.join(name);
            if file.is_file() {
                return vec![file_url(&file)];
            }
        }
        return Vec::new();
    }
    let (name, version) = parse_package(&spec.target);
    let at = match version {
        Some(version) => format!("npm:{name}@{version}"),
        None => format!("npm:{name}@latest"),
    };
    vec![format!("{at}/tui")]
}

fn local_directory_entry(dir: &Path) -> Option<PathBuf> {
    let manifest = std::fs::read_to_string(dir.join("package.json")).ok();
    if let Some(text) = manifest {
        if let Ok(v) = serde_json::from_str::<Value>(crate::util::strip_bom(&text).0) {
            if let Some(found) = package_entry(dir, &v) {
                return Some(found);
            }
        }
    }
    for name in ["index.ts", "index.tsx", "index.js", "index.mjs", "index.cjs"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn package_entry(dir: &Path, manifest: &Value) -> Option<PathBuf> {
    let path_in = |raw: &str| -> Option<PathBuf> {
        let raw = raw.strip_prefix("file://").unwrap_or(raw);
        let p = if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { dir.join(raw) };
        let p = crate::util::normalize(&p);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    let export_string = |value: &Value| -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => ["import", "default"].iter().find_map(|k| o.get(*k).and_then(Value::as_str).map(String::from)),
            _ => None,
        }
    };
    if let Some(exports) = manifest.get("exports") {
        if let Some(server) = exports.get("./server").and_then(&export_string) {
            if let Some(p) = path_in(&server) {
                return Some(p);
            }
        }
        if let Some(root) = exports.get(".").and_then(&export_string) {
            if let Some(p) = path_in(&root) {
                return Some(p);
            }
        } else if let Some(root) = export_string(exports) {
            if let Some(p) = path_in(&root) {
                return Some(p);
            }
        }
    }
    if let Some(main) = manifest.get("main").and_then(Value::as_str) {
        if let Some(p) = path_in(main) {
            return Some(p);
        }
    }
    None
}

fn file_url(path: &Path) -> String {
    url::Url::from_file_path(crate::util::normalize(path))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file:///{}", path.to_string_lossy().replace('\\', "/").trim_start_matches('/')))
}

fn package_name(spec: &PluginSpec) -> String {
    if is_local(&spec.target) {
        let p = local_path(&spec.target);
        if p.is_dir() {
            if let Ok(text) = std::fs::read_to_string(p.join("package.json")) {
                if let Ok(v) = serde_json::from_str::<Value>(crate::util::strip_bom(&text).0) {
                    if let Some(name) = v.get("name").and_then(Value::as_str) {
                        return name.to_string();
                    }
                }
            }
            return p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "plugin".into());
        }
        return p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "plugin".into());
    }
    parse_package(&spec.target).0
}

// ---------------------------------------------------------------- deno runtime

fn deno_binary_name() -> &'static str {
    if cfg!(windows) {
        "deno.exe"
    } else {
        "deno"
    }
}

fn deno_asset() -> Option<&'static str> {
    if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "aarch64") {
        Some("aarch64-pc-windows-msvc")
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        Some("aarch64-apple-darwin")
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        Some("x86_64-apple-darwin")
    } else {
        None
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub async fn ensure_deno() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("OPENCODE_DENO") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(p) = find_on_path("deno").or_else(|| find_on_path("deno.exe")) {
        return Ok(p);
    }
    let target = crate::paths::get().cache.join("bin").join(deno_binary_name());
    if target.is_file() {
        return Ok(target);
    }
    let asset = deno_asset().ok_or_else(|| anyhow!("Deno has no prebuilt binary for this platform; install Deno or set OPENCODE_DENO"))?;
    let url = format!("https://github.com/denoland/deno/releases/download/v{DENO_VERSION}/deno-{asset}.zip");
    let response = crate::http::send(crate::http::client().get(&url).timeout(std::time::Duration::from_secs(180)))
        .await
        .with_context(|| format!("downloading Deno from {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading Deno from {url}"))?;
    let bytes = response.bytes().await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading Deno archive")?;
    let mut file = archive.by_name(deno_binary_name()).context("Deno binary missing from archive")?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = target.with_extension("download");
    {
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut file, &mut out)?;
    }
    std::fs::rename(&tmp, &target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
    }
    Ok(target)
}

/// Bare imports in local plugins resolve through a generated import map. Read
/// each plugin's nearest package.json so dependencies declared there work
/// without creating node_modules in the user's project.
fn plugin_dependencies(specs: &[PluginSpec]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for spec in specs {
        if !is_local(&spec.target) {
            continue;
        }
        let path = local_path(&spec.target);
        let start = if path.is_dir() { path } else { path.parent().map(Path::to_path_buf).unwrap_or(path) };
        let mut current = Some(start);
        for _ in 0..4 {
            let Some(dir) = current else { break };
            let manifest = dir.join("package.json");
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                if let Ok(value) = serde_json::from_str::<Value>(crate::util::strip_bom(&text).0) {
                    for key in ["dependencies", "optionalDependencies"] {
                        if let Some(map) = value.get(key).and_then(Value::as_object) {
                            for (name, version) in map {
                                let Some(version) = version.as_str() else { continue };
                                if version.starts_with("workspace:")
                                    || version.starts_with("file:")
                                    || version.starts_with("link:")
                                {
                                    continue;
                                }
                                out.entry(name.clone()).or_insert_with(|| version.to_string());
                            }
                        }
                    }
                }
                break;
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }
    out
}

fn runtime_dir() -> PathBuf {
    crate::paths::get().cache.join("plugin-runtime")
}

fn write_runtime(extra_dependencies: &BTreeMap<String, String>) -> Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let host = dir.join("host.ts");
    if std::fs::read_to_string(&host).ok().as_deref() != Some(HOST_TS) {
        std::fs::write(&host, HOST_TS)?;
    }
    let mut dependencies = Map::new();
    for (name, version) in extra_dependencies {
        let version = if version.trim().is_empty() || version.trim() == "*" { "latest" } else { version };
        dependencies.insert(name.clone(), Value::String(format!("npm:{name}@{version}")));
        dependencies.insert(format!("{name}/"), Value::String(format!("npm:{name}@{version}/")));
    }
    let map = dir.join("import_map.json");
    let mut imports = json!({
        "imports": {
            "effect": format!("npm:effect@{EFFECT_VERSION}"),
            "effect/": format!("npm:effect@{EFFECT_VERSION}/"),
            "@opencode/plugin": format!("npm:@opencode/plugin@{PLUGIN_API_VERSION}"),
            "@opencode/plugin/": format!("npm:@opencode/plugin@{PLUGIN_API_VERSION}/"),
            "@opencode/schema": format!("npm:@opencode/schema@{PLUGIN_API_VERSION}"),
            "@opencode/schema/": format!("npm:@opencode/schema@{PLUGIN_API_VERSION}/"),
            "@opencode/client": format!("npm:@opencode/client@{PLUGIN_API_VERSION}"),
            "@opencode/client/": format!("npm:@opencode/client@{PLUGIN_API_VERSION}/"),
            "@opencode/util": format!("npm:@opencode/util@{PLUGIN_API_VERSION}"),
            "@opencode/util/": format!("npm:@opencode/util@{PLUGIN_API_VERSION}/"),
            "@opencode/ai": format!("npm:@opencode/ai@{PLUGIN_API_VERSION}"),
            "@opencode/ai/": format!("npm:@opencode/ai@{PLUGIN_API_VERSION}/"),
            "@opencode/protocol": format!("npm:@opencode/protocol@{PLUGIN_API_VERSION}"),
            "@opencode/protocol/": format!("npm:@opencode/protocol@{PLUGIN_API_VERSION}/"),
            "@opencode-ai/plugin": format!("npm:@opencode-ai/plugin@{V1_PLUGIN_API_VERSION}"),
            "@opencode-ai/plugin/": format!("npm:@opencode-ai/plugin@{V1_PLUGIN_API_VERSION}/"),
            "@opencode-ai/sdk": format!("npm:@opencode-ai/sdk@{V1_PLUGIN_API_VERSION}"),
            "@opencode-ai/sdk/": format!("npm:@opencode-ai/sdk@{V1_PLUGIN_API_VERSION}/")
        }
    });
    if let Some(values) = imports.get_mut("imports").and_then(Value::as_object_mut) {
        // Deno import maps cannot append a suffix to an `npm:` prefix, so
        // package subpaths need exact entries.
        const PLUGIN_SUBPATHS: &[&str] = &[
            "effect", "effect/agent", "effect/aisdk", "effect/artifact", "effect/command", "effect/event",
            "effect/index", "effect/integration", "effect/mcp", "effect/model", "effect/permission",
            "effect/plugin", "effect/provider", "effect/reference", "effect/registration", "effect/rpc",
            "effect/session", "effect/shell", "effect/skill", "effect/storage", "effect/tool", "effect/vcs",
            "effect/websearch", "effect/worktree", "promise/agent", "promise/aisdk", "promise/artifact",
            "promise/command", "promise/event", "promise/index", "promise/integration", "promise/mcp",
            "promise/model", "promise/permission", "promise/plugin", "promise/provider", "promise/reference",
            "promise/registration", "promise/rpc", "promise/session", "promise/shell", "promise/skill",
            "promise/storage", "promise/tool", "promise/vcs", "promise/websearch", "promise/worktree",
            "host", "options", "permission", "rpc", "source", "storage", "worktree", "runtime", "tui",
            "gui", "gui/main", "gui/file", "gui/review", "gui/browser", "gui/vite",
        ];
        for path in PLUGIN_SUBPATHS {
            values.insert(
                format!("@opencode/plugin/{path}"),
                Value::String(format!("npm:@opencode/plugin@{PLUGIN_API_VERSION}/{path}")),
            );
        }
        const SCHEMA_MODULES: &[&str] = &[
            "agent", "artifact", "command", "config", "connection", "credential", "event", "event-manifest",
            "file-diff", "filesystem", "form", "generate", "goal", "identifier", "index", "instruction",
            "instruction-entry", "integration", "integration-id", "llm", "location", "mcp", "model",
            "model-compatibility", "money", "permission", "persistent-pty", "plugin", "project", "project-id",
            "prompt", "prompt-input", "provider", "pty", "question", "reference", "rpc", "schema", "server-event",
            "session", "session-error", "session-event", "session-fork", "session-id", "session-inbox",
            "session-message", "session-metadata", "session-revert", "session-status-event", "shell", "skill",
            "snapshot", "token-usage", "tool", "vcs", "websearch", "workspace", "workspace-id", "worktree",
        ];
        for path in SCHEMA_MODULES {
            values.insert(
                format!("@opencode/schema/{path}"),
                Value::String(format!("npm:@opencode/schema@{PLUGIN_API_VERSION}/{path}")),
            );
        }
        const CLIENT_MODULES: &[&str] = &[
            "contract", "mcp", "shared-events", "effect/api", "effect/client", "effect/index", "effect/rpc",
            "effect/service", "promise/api", "promise/client", "promise/index", "promise/media", "promise/rpc",
            "promise/service", "solid/data", "solid/index",
        ];
        for path in CLIENT_MODULES {
            values.insert(
                format!("@opencode/client/{path}"),
                Value::String(format!("npm:@opencode/client@{PLUGIN_API_VERSION}/{path}")),
            );
        }
        const EFFECT_MODULES: &[&str] = &[
            "Arbitrary", "Array", "BigDecimal", "BigInt", "Boolean", "Brand", "Bytes", "ByteSize", "Cache",
            "Cause", "Channel", "ChannelSchema", "Chunk", "Clock", "Combiner", "Config", "ConfigProvider",
            "Console", "Context", "Cron", "Data", "DateTime", "Deferred", "Differ", "Duration", "Effect",
            "Effectable", "Equal", "Equivalence", "ErrorReporter", "ExecutionPlan", "Exit", "Fiber",
            "FiberHandle", "FiberMap", "FiberSet", "FileSystem", "Filter", "Formatter", "Function", "Glob",
            "Graph", "Hash", "HashMap", "HashRing", "HashSet", "HKT", "Inspectable", "Iterable", "JsonPatch",
            "JsonPointer", "JsonSchema", "Latch", "Layer", "LayerMap", "LayerRef", "Logger", "LogLevel",
            "ManagedRuntime", "Match", "Metric", "MutableHashMap", "MutableHashSet", "MutableList", "MutableRef",
            "Newtype", "NonEmptyIterable", "Number", "Optic", "Option", "Order", "Ordering",
            "PartitionedSemaphore", "Path", "Pipeable", "PlatformError", "Pool", "Predicate", "PrimaryKey",
            "PubSub", "Pull", "Queue", "Random", "RcMap", "RcRef", "Record", "Redactable", "Redacted", "Reducer",
            "Ref", "References", "RegExp", "Request", "RequestResolver", "Resource", "Result", "Runtime",
            "Schedule", "Scheduler", "Schema", "SchemaAST", "SchemaGetter", "SchemaIssue", "SchemaParser",
            "SchemaRepresentation", "SchemaTransformation", "Scope", "ScopedCache", "ScopedRef", "Semaphore",
            "SemVer", "SemVerRange", "Sink", "StandardSchema", "Stdio", "Stream", "String", "Struct",
            "SubscriptionRef", "Symbol", "SynchronizedRef", "Take", "Terminal", "TextDiff", "Tracer", "Trie",
            "Tuple", "TxChunk", "TxDeferred", "TxHashMap", "TxHashSet", "TxPriorityQueue", "TxPubSub", "TxQueue",
            "TxReentrantLock", "TxRef", "TxSemaphore", "TxSubscriptionRef", "Types", "UndefinedOr", "Unify",
            "Utils", "Version", "ai", "cli", "cluster", "crypto", "devtools", "encoding", "eventlog", "graphql",
            "http", "http-api", "net", "observability", "persistence", "process", "reactivity", "rpc", "schema",
            "socket", "sql", "testing", "workflow", "workers",
        ];
        for path in EFFECT_MODULES {
            values.insert(
                format!("effect/{path}"),
                Value::String(format!("npm:effect@{EFFECT_VERSION}/{path}")),
            );
        }
        const V1_PLUGIN_SUBPATHS: &[&str] = &[
            "tool", "tui", "v2/promise", "v2/effect", "v2/effect/plugin", "v2/effect/integration",
        ];
        for path in V1_PLUGIN_SUBPATHS {
            values.insert(
                format!("@opencode-ai/plugin/{path}"),
                Value::String(format!("npm:@opencode-ai/plugin@{V1_PLUGIN_API_VERSION}/{path}")),
            );
        }
        for path in ["hash", "runtime-import", "fs-util", "npm", "effect/app-node", "effect/layer-node"] {
            values.insert(
                format!("@opencode/util/{path}"),
                Value::String(format!("npm:@opencode/util@{PLUGIN_API_VERSION}/{path}")),
            );
        }
        for (name, value) in dependencies {
            // The host owns these modules: plugins and their dependencies must
            // share one Effect and one @opencode/plugin instance.
            if name == "effect"
                || name == "@opencode/plugin"
                || name.starts_with("@opencode/")
                || name.starts_with("@opencode-ai/")
            {
                continue;
            }
            values.insert(name, value);
        }
    }
    let text = serde_json::to_string_pretty(&imports)?;
    if std::fs::read_to_string(&map).ok().as_deref() != Some(&text) {
        std::fs::write(&map, text)?;
    }
    Ok(dir)
}

// ---------------------------------------------------------------- manager

#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub source: String,
    pub permission: Option<String>,
}

pub struct Manager {
    inner: Option<Arc<Inner>>,
    tools: RwLock<Vec<ToolDescriptor>>,
    info: RwLock<Vec<Value>>,
    websearch: RwLock<Vec<(String, String)>>,
    websearch_default: RwLock<Option<Value>>,
    tui: RwLock<Vec<Value>>,
    errors: Vec<String>,
    events: tokio::sync::broadcast::Sender<Value>,
}

struct Inner {
    tx: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next: AtomicU64,
    app: Arc<Mutex<Option<Weak<crate::app::App>>>>,
    child: Mutex<Option<tokio::process::Child>>,
    shutdown: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(mut child) = self.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_specifiers() {
        assert_eq!(parse_package("pkg"), ("pkg".into(), None));
        assert_eq!(parse_package("pkg@1.2.3"), ("pkg".into(), Some("1.2.3".into())));
        assert_eq!(parse_package("@scope/pkg"), ("@scope/pkg".into(), None));
        assert_eq!(parse_package("@scope/pkg@1.2.3"), ("@scope/pkg".into(), Some("1.2.3".into())));
    }

    #[test]
    fn collects_native_legacy_and_removals() {
        let config = json!({
            "plugins": ["a", { "package": "b", "options": { "x": 1 } }, "-a"],
            "plugin": ["c", ["d", { "y": 2 }]]
        });
        let specs = collect_specs(&config, &[]);
        let targets: Vec<&str> = specs.iter().map(|s| s.target.as_str()).collect();
        assert_eq!(targets, vec!["b", "c", "d"]);
        assert_eq!(specs[0].options["x"], 1);
        assert_eq!(specs[2].options["y"], 2);
    }

    #[test]
    fn later_declaration_wins() {
        let config = json!({ "plugins": ["a", "a"] });
        assert_eq!(collect_specs(&config, &[]).len(), 1);
    }

    #[test]
    fn resolves_relative_plugin_paths() {
        let dir = std::env::temp_dir().join("opencode-plugin-path-test");
        let mut value = json!({ "plugins": ["./plugin.ts", { "package": "../shared" }] });
        absolutize_config(&mut value, &dir);
        let first = value["plugins"][0].as_str().unwrap();
        assert!(Path::new(first).is_absolute());
        assert!(first.ends_with("plugin.ts"));
        let second = value["plugins"][1]["package"].as_str().unwrap();
        assert!(Path::new(second).is_absolute());
    }

    #[test]
    fn history_projects_records_and_synthetic_messages() {
        let mut synthetic = crate::session::Message::user_text("");
        synthetic.time = 1;
        synthetic.parts = vec![crate::session::Part::Text { text: "note".into(), synthetic: true }];
        let mut user = crate::session::Message::user_text("hello");
        user.time = 2;
        let records = vec![json!({
            "id": "msg_record",
            "time": { "created": 3 },
            "type": "agent-switched",
            "agent": "build",
        })];
        let history = session_history_json(&[synthetic, user], &[], &records);
        let history = history.as_array().unwrap();
        assert_eq!(history[0]["type"], "synthetic");
        assert_eq!(history[1]["type"], "user");
        assert_eq!(history[2]["type"], "agent-switched");
        assert_eq!(history[2]["id"], "msg_record");
    }

    #[test]
    fn message_hook_round_trips_text() {
        let mut messages = vec![crate::session::Message::user_text("hello")];
        let value = messages_for_hook("ses_test", &messages);
        assert_eq!(value[0]["info"]["role"], "user");
        let mut changed = value.clone();
        changed[0]["parts"][0]["text"] = json!("goodbye");
        apply_messages_hook(&mut messages, &changed);
        assert_eq!(messages[0].text(), "goodbye");
    }

    #[test]
    fn empty_plugin_state_removes_everything() {
        let config = crate::config::Config::default();
        let agents = crate::agent::load(&config, &[]);
        assert!(!agents.is_empty());
        let applied = apply_state(
            &json!({
                "agents": [],
                "providers": [],
                "mcp": [],
                "skills": [],
                "commands": [],
            }),
            &config,
            &Default::default(),
            &agents,
            &Default::default(),
        );
        assert!(applied.agents.is_empty());
        assert!(applied.providers.is_empty());
        assert!(applied.mcp.is_empty());
        assert!(applied.skills.is_empty());
    }

    #[test]
    fn file_diffs_collect_tool_metadata() {
        let mut message = crate::session::Message::new(crate::session::Role::Assistant);
        message.parts.push(crate::session::Part::Tool {
            id: "call".into(),
            name: "edit".into(),
            input: json!({}),
            state: crate::session::ToolState::Completed {
                output: String::new(),
                title: String::new(),
                metadata: json!({
                    "files": [{
                        "file": "src/main.rs",
                        "diff": "--- a/src/main.rs\n+++ b/src/main.rs\n",
                        "additions": 2,
                        "deletions": 1,
                    }]
                }),
                attachments: vec![],
            },
            provider_meta: None,
        });
        let diffs = file_diffs(&[message]);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0]["file"], "src/main.rs");
        assert_eq!(diffs[0]["status"], "modified");
    }

    #[test]
    fn prompt_attachments_render_agents_and_skills() {
        let skills = vec![crate::tools::skill::Skill {
            id: "test".into(),
            name: "Test".into(),
            description: String::new(),
            path: PathBuf::new(),
            content: "follow the test".into(),
        }];
        let body = json!({
            "agents": [{ "name": "reviewer" }],
            "skills": [{ "id": "test" }, { "name": "inline", "text": "extra instructions" }],
        });
        let text = prompt_attachments(&skills, &body);
        assert!(text.contains("[Agent mention: reviewer]"));
        assert!(text.contains("[Skill activated: Test]\nfollow the test"));
        assert!(text.contains("[Skill activated: inline]\nextra instructions"));
    }

    #[test]
    fn shell_messages_use_v2_shapes() {
        let shell = crate::session::ShellMessage {
            id: "sh_1".into(),
            command: "echo hi".into(),
            status: "exited".into(),
            exit: Some(0),
            output: Some("hi\n".into()),
            started: 1,
            completed: Some(2),
        };
        let value = shell_message_json(&shell);
        assert_eq!(value["type"], "shell");
        assert_eq!(value["shellID"], "sh_1");
        assert_eq!(value["status"], "exited");
        assert_eq!(value["output"]["output"], "hi\n");
    }

    #[test]
    fn compaction_messages_use_v2_shapes() {
        let mut message = crate::session::Message::new(crate::session::Role::User);
        message.parts.push(crate::session::Part::Compaction { summary: "summary".into(), metadata: None });
        message.usage.input = 10;
        let value = session_messages_json(&[message]);
        assert_eq!(value[0]["type"], "compaction");
        assert_eq!(value[0]["status"], "completed");
        assert_eq!(value[0]["summary"], "summary");
        assert_eq!(value[0]["tokens"]["input"], 10);
    }

    #[test]
    fn session_messages_use_v2_shapes() {
        let messages = vec![crate::session::Message::user_text("hello")];
        let value = session_messages_json(&messages);
        assert_eq!(value[0]["type"], "user");
        assert_eq!(value[0]["text"], "hello");
        assert_eq!(value[0]["content"], Value::Null);
    }

    #[test]
    fn hook_can_add_synthetic_messages() {
        let mut messages = vec![crate::session::Message::user_text("yo")];
        let value = json!([
            {
                "info": { "id": "__magic_context_v2_m0__", "role": "user" },
                "parts": [{ "type": "text", "text": "<session-history></session-history>" }]
            },
            {
                "info": { "id": "__magic_context_v2_m1__", "role": "user" },
                "parts": [{ "type": "text", "text": "<project-memory></project-memory>" }]
            },
            {
                "info": { "id": "user-id", "role": "user" },
                "parts": [{ "type": "text", "text": "yo" }]
            }
        ]);
        apply_messages_hook(&mut messages, &value);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].id, "__magic_context_v2_m0__");
        assert_eq!(messages[1].id, "__magic_context_v2_m1__");
        assert_eq!(messages[2].text(), "yo");
    }

    #[test]
    fn hook_can_replace_and_remove_messages() {
        let mut messages = vec![crate::session::Message::user_text("original")];
        let id = messages[0].id.clone();
        let value = json!([
            {
                "info": { "id": id, "role": "user" },
                "parts": [{ "type": "text", "text": "rewritten" }]
            }
        ]);
        apply_messages_hook(&mut messages, &value);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "rewritten");

        let drop = json!([]);
        apply_messages_hook(&mut messages, &drop);
        assert_eq!(messages.len(), 1, "an empty transform must not erase the request");
    }
}

impl Manager {
    pub fn disabled() -> Manager {
        Manager {
            inner: None,
            tools: RwLock::new(Vec::new()),
            info: RwLock::new(Vec::new()),
            websearch: RwLock::new(Vec::new()),
            websearch_default: RwLock::new(None),
            tui: RwLock::new(Vec::new()),
            errors: Vec::new(),
            events: tokio::sync::broadcast::channel(4096).0,
        }
    }

    fn disabled_with(errors: Vec<String>) -> Manager {
        Manager {
            inner: None,
            tools: RwLock::new(Vec::new()),
            info: RwLock::new(Vec::new()),
            websearch: RwLock::new(Vec::new()),
            websearch_default: RwLock::new(None),
            tui: RwLock::new(Vec::new()),
            errors,
            events: tokio::sync::broadcast::channel(4096).0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn list(&self) -> Vec<Value> {
        self.info.read().unwrap().clone()
    }

    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.read().unwrap().clone()
    }

    pub fn attach(&self, app: &Arc<crate::app::App>) {
        if let Some(inner) = &self.inner {
            *inner.app.lock().unwrap() = Some(Arc::downgrade(app));
        }
    }

    pub fn emit(&self, mut event: Value) {
        crate::event_store::append(&mut event);
        let _ = self.events.send(event.clone());
        let Some(inner) = &self.inner else { return };
        let _ = inner.tx.send(json!({ "type": "notification", "method": "event", "params": event }));
    }

    /// Invoke a plugin-registered RPC method through the Deno host.
    pub async fn rpc_call(&self, rpc_id: &str, method: &str, input: Value) -> Result<Value> {
        self.request(
            "rpc.call",
            json!({ "rpcID": rpc_id, "method": method, "input": input }),
        )
        .await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, Some(std::time::Duration::from_secs(120))).await
    }

    async fn request_with_timeout(&self, method: &str, params: Value, timeout: Option<std::time::Duration>) -> Result<Value> {
        let Some(inner) = &self.inner else { return Ok(Value::Null) };
        let id = inner.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().unwrap().insert(id, tx);
        if inner.tx.send(json!({ "type": "request", "id": id, "method": method, "params": params })).is_err() {
            inner.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("plugin host is not running"));
        }
        let received = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, rx).await.map_err(|_| ()),
            None => Ok(rx.await),
        };
        match received {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow!("plugin host closed")),
            Err(_) => {
                inner.pending.lock().unwrap().remove(&id);
                Err(anyhow!("plugin host timed out on {method}"))
            }
        }
    }

    pub async fn trigger(&self, domain: &str, name: &str, event: Value) -> Value {
        if !self.enabled() {
            return event;
        }
        match self.request("hook.trigger", json!({ "domain": domain, "name": name, "event": event })).await {
            Ok(value) if value.is_object() => value,
            Ok(_) => event,
            Err(error) => {
                crate::util::log(&format!("plugin hook {domain}.{name} failed: {error}"));
                event
            }
        }
    }

    pub async fn tool_list(&self) -> Vec<ToolDescriptor> {
        if self.enabled() {
            if let Ok(Value::Array(items)) = self.request("tool.list", json!({})).await {
                let mut out = Vec::new();
                for item in items {
                    if let Some(tool) = descriptor_from_value(&item) {
                        out.push(tool);
                    }
                }
                *self.tools.write().unwrap() = out.clone();
                return out;
            }
        }
        self.tool_descriptors()
    }

    pub fn abort_tool(&self, call_id: &str) {
        if let Some(inner) = &self.inner {
            let _ = inner.tx.send(json!({
                "type": "notification",
                "method": "tool.abort",
                "params": { "callID": call_id }
            }));
        }
    }

    pub async fn execute_tool(&self, name: &str, call_id: &str, input: Value, context: Value) -> Result<Value> {
        self.request_with_timeout(
            "tool.execute",
            json!({ "name": name, "callID": call_id, "input": input, "context": context }),
            None,
        )
        .await
    }

    pub async fn execute_command(&self, name: &str, session_id: &str, prompt: Value, delivery: &str) -> Result<Value> {
        self.request_with_timeout(
            "command.execute",
            json!({ "name": name, "sessionID": session_id, "prompt": prompt, "delivery": delivery }),
            None,
        )
        .await
    }

    fn apply_runtime_state(&self, state: &Value) {
        if let Some(Value::Array(tools)) = state.get("tools") {
            let mut out = Vec::new();
            for item in tools {
                if let Some(tool) = descriptor_from_value(item) {
                    out.push(tool);
                }
            }
            *self.tools.write().unwrap() = out;
        }
        if let Some(Value::Array(providers)) = state.get("websearch") {
            *self.websearch.write().unwrap() = providers
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get("id").and_then(Value::as_str)?.to_string(),
                        item.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    ))
                })
                .collect();
        }
        *self.websearch_default.write().unwrap() = state
            .get("websearchDefault")
            .filter(|value| value.is_string() || value.is_boolean())
            .cloned();
        if let Some(Value::Array(commands)) = state.pointer("/tui/commands") {
            *self.tui.write().unwrap() = commands.clone();
        }
    }

    pub fn tui_commands(&self) -> Vec<Value> {
        self.tui.read().unwrap().clone()
    }

    pub async fn execute_tui_command(&self, name: &str, input: Value) -> Result<Value> {
        self.request("tui.command.execute", json!({ "name": name, "input": input })).await
    }

    pub fn websearch_providers(&self) -> Vec<(String, String)> {
        self.websearch.read().unwrap().clone()
    }

    pub fn has_websearch(&self) -> bool {
        !self.websearch.read().unwrap().is_empty()
    }

    pub fn websearch_provider(&self, requested: Option<&str>) -> Option<String> {
        let providers = self.websearch.read().unwrap();
        if let Some(requested) = requested {
            if providers.iter().any(|(id, _)| id == requested) {
                return Some(requested.to_string());
            }
        }
        match self.websearch_default.read().unwrap().as_ref() {
            Some(Value::String(id)) => Some(id.clone()),
            Some(Value::Bool(false)) => None,
            _ => providers.first().map(|(id, _)| id.clone()),
        }
    }

    pub async fn execute_websearch(&self, provider_id: &str, query: &str) -> Result<Value> {
        self.request("websearch.execute", json!({ "providerID": provider_id, "query": query })).await
    }

    /// Wait for event hooks already dispatched to the host.
    pub async fn flush(&self) {
        if self.enabled() {
            let _ = self.request_with_timeout("flush", json!({}), Some(std::time::Duration::from_secs(10))).await;
        }
    }

    /// Run plugin cleanups and stop the Deno host.
    pub async fn shutdown(&self) {
        crate::provider::ws_transport::close_all().await;
        let Some(inner) = &self.inner else { return };
        let _ = self.request_with_timeout("shutdown", json!({}), Some(std::time::Duration::from_secs(10))).await;
        inner.shutdown.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(mut child) = inner.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }

}

fn descriptor_from_value(item: &Value) -> Option<ToolDescriptor> {
    let name = item.get("name").and_then(Value::as_str)?;
    Some(ToolDescriptor {
        id: item.get("id").and_then(Value::as_str).unwrap_or(name).to_string(),
        name: name.to_string(),
        description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
        schema: item.get("input").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        source: item.get("source").and_then(Value::as_str).unwrap_or("custom").to_string(),
        permission: item.pointer("/options/permission").and_then(Value::as_str).map(String::from),
    })
}

/// Start the Deno host and return the manager plus the transformed plugin state.
pub async fn init(
    directory: &Path,
    worktree: &Path,
    project: &str,
    specs: Vec<PluginSpec>,
    snapshot: Value,
    setup_app: Option<&Arc<crate::app::App>>,
) -> (Manager, Value, Vec<String>) {
    if specs.is_empty() || std::env::var_os("OPENCODE_DISABLE_PLUGINS").is_some() {
        return (Manager::disabled(), snapshot, Vec::new());
    }
    match start(directory, worktree, project, specs, snapshot.clone(), setup_app).await {
        Ok((manager, state)) => {
            manager.apply_runtime_state(&state);
            (manager, state, Vec::new())
        }
        Err(error) => {
            let message = format!("plugins disabled: {error:#}");
            crate::util::log(&message);
            (Manager::disabled_with(vec![message.clone()]), snapshot, vec![message])
        }
    }
}

async fn start(
    directory: &Path,
    worktree: &Path,
    project: &str,
    specs: Vec<PluginSpec>,
    snapshot: Value,
    setup_app: Option<&Arc<crate::app::App>>,
) -> Result<(Manager, Value)> {
    let deno = ensure_deno().await?;
    let dependencies = plugin_dependencies(&specs);
    let dir = write_runtime(&dependencies)?;
    let host = dir.join("host.ts");
    let map = dir.join("import_map.json");
    let mut plugins = Vec::new();
    let mut seen = BTreeSet::new();
    let mut skipped = Vec::new();
    for spec in &specs {
        let tui_candidates = tui_entry_candidates(spec);
        let candidates = match entry_candidates(spec) {
            Ok(candidates) => candidates,
            Err(error) => {
                if tui_candidates.is_empty() {
                    crate::util::log(&format!("skipping plugin {}: {error:#}", spec.target));
                    skipped.push(format!("skipping plugin {}: {error:#}", spec.target));
                    continue;
                }
                Vec::new()
            }
        };
        let id = package_name(spec);
        let mut id = id;
        if !seen.insert(id.clone()) {
            id = format!("{id}@{}", plugins.len());
            seen.insert(id.clone());
        }
        plugins.push(json!({
            "id": id,
            "spec": spec.target,
            "options": spec.options,
            "source": if is_local(&spec.target) {
                json!({ "type": "local", "path": spec.target })
            } else {
                let (name, version) = parse_package(&spec.target);
                json!({ "type": "package", "target": name, "version": version })
            },
            "candidates": candidates,
            "tuiCandidates": tui_candidates,
        }));
    }
    if plugins.is_empty() {
        return Err(anyhow!("{}", skipped.join("\n")));
    }
    let config_path = crate::paths::get()
        .cache
        .join("plugin-runtime")
        .join(format!("config-{}-{}.json", std::process::id(), crate::util::id("run")));
    let shell = crate::tools::shell::detect();
    let config = json!({
        "directory": directory.to_string_lossy(),
        "worktree": worktree.to_string_lossy(),
        "project": project,
        "version": crate::VERSION,
        "channel": crate::UPSTREAM_CHANNEL,
        "shell": shell.program,
        "storageDir": crate::paths::get().data.join("storage").join("plugin").join("kv").to_string_lossy(),
        "plugins": plugins,
        "state": snapshot,
        "tui": std::env::var_os("OPENCODE_TUI").is_some(),
    });
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
    }

    let mut command = tokio::process::Command::new(&deno);
    command
        .arg("run")
        .arg("--allow-all")
        .arg("--no-prompt")
        .arg("--quiet")
        .arg("--no-config")
        .arg("--no-lock")
        .arg(format!("--import-map={}", map.display()))
        .arg(&host)
        .arg(&config_path)
        .current_dir(directory)
        .env("DENO_DIR", crate::paths::get().cache.join("deno"))
        .env("DENO_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .env("OPENCODE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().with_context(|| format!("starting {}", deno.display()))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("plugin host stdin unavailable"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("plugin host stdout unavailable"))?;
    let stderr = child.stderr.take();

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(message) = rx.recv().await {
            let line = match serde_json::to_vec(&message) {
                Ok(line) => line,
                Err(_) => continue,
            };
            if stdin.write_all(&line).await.is_err() {
                break;
            }
            if stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> = Arc::new(Mutex::new(HashMap::new()));
    let app = Arc::new(Mutex::new(None::<Weak<crate::app::App>>));
    let shutdown = CancellationToken::new();
    let manager = Manager {
        inner: Some(Arc::new(Inner {
            tx: tx.clone(),
            pending: pending.clone(),
            next: AtomicU64::new(1),
            app: app.clone(),
            child: Mutex::new(Some(child)),
            shutdown: shutdown.clone(),
        })),
        tools: RwLock::new(Vec::new()),
        info: RwLock::new(Vec::new()),
        websearch: RwLock::new(Vec::new()),
        websearch_default: RwLock::new(None),
        tui: RwLock::new(Vec::new()),
        errors: Vec::new(),
        events: tokio::sync::broadcast::channel(4096).0,
    };

    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                crate::util::log(&format!("[deno] {line}"));
            }
        });
    }

    // reader
    {
        let pending = pending.clone();
        let tx = tx.clone();
        let app = app.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                match message.get("type").and_then(Value::as_str) {
                    Some("response") => {
                        let id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let Some(sender) = pending.lock().unwrap().remove(&id) else { continue };
                        let result = if message.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(anyhow!(
                                "{}",
                                message.pointer("/error/message").and_then(Value::as_str).unwrap_or("plugin host error")
                            ))
                        };
                        let _ = sender.send(result);
                    }
                    Some("request") => {
                        let tx = tx.clone();
                        let app = app.clone();
                        tokio::spawn(async move {
                            let id = message.get("id").cloned().unwrap_or(Value::Null);
                            let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
                            let params = message.get("params").cloned().unwrap_or(Value::Null);
                            let cancel_id = params
                                .pointer("/options/cancelID")
                                .and_then(Value::as_str)
                                .map(String::from);
                            let token = cancel_id.as_ref().map(|cancel_id| {
                                let token = CancellationToken::new();
                                client_cancels().lock().unwrap().insert(cancel_id.clone(), token.clone());
                                token
                            });
                            let result = if let Some(token) = token.clone() {
                                tokio::select! {
                                    result = host_call(&method, params, &app) => result,
                                    _ = token.cancelled() => Err(anyhow!("plugin request aborted")),
                                }
                            } else {
                                host_call(&method, params, &app).await
                            };
                            if let Some(cancel_id) = cancel_id {
                                client_cancels().lock().unwrap().remove(&cancel_id);
                            }
                            let response = match result {
                                Ok(result) => json!({ "type": "response", "id": id, "ok": true, "result": result }),
                                Err(error) => json!({
                                    "type": "response",
                                    "id": id,
                                    "ok": false,
                                    "error": { "message": format!("{error:#}") }
                                }),
                            };
                            let _ = tx.send(response);
                        });
                    }
                    Some("notification") => {
                        let current = app.lock().unwrap().as_ref().and_then(Weak::upgrade);
                        if let Some(app) = current {
                            match message.get("method").and_then(Value::as_str) {
                                Some("state.changed") => {
                                    if let Some(state) = message.pointer("/params/state") {
                                        app.apply_plugin_state(state);
                                    }
                                }
                                Some("tool.metadata") => {
                                    let call_id = message.pointer("/params/callID").and_then(Value::as_str).unwrap_or_default();
                                    let metadata = message
                                        .pointer("/params/metadata")
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            let mut value = message.get("params").cloned().unwrap_or(Value::Null);
                                            if let Some(map) = value.as_object_mut() {
                                                map.remove("callID");
                                            }
                                            value
                                        });
                                    app.record_tool_progress(call_id, metadata);
                                }
                                Some("client.cancel") => {
                                    let cancel_id = message.pointer("/params/cancelID").and_then(Value::as_str);
                                    if let Some(token) = cancel_id.and_then(|id| client_cancels().lock().unwrap().get(id).cloned()) {
                                        token.cancel();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            for (_, sender) in pending.lock().unwrap().drain() {
                let _ = sender.send(Err(anyhow!("plugin host exited")));
            }
            shutdown.cancel();
        });
    }

    // Give the host a moment to install its runtime and import plugin modules.
    // The caller may attach a provisional app first so setup can use client APIs.
    if let Some(app) = setup_app {
        manager.attach(app);
    }
    let ready = manager
        .request_with_timeout("initialize", json!({}), Some(std::time::Duration::from_secs(300)))
        .await;
    let _ = std::fs::remove_file(&config_path);
    let ready = ready?;
    let state = ready.get("state").cloned().unwrap_or(snapshot);
    if let Some(Value::Array(items)) = ready.get("plugins") {
        *manager.info.write().unwrap() = items.clone();
    }
    if let Some(Value::Array(errors)) = ready.get("errors") {
        for error in errors {
            crate::util::log(&format!(
                "plugin {} failed: {}",
                error.get("id").and_then(Value::as_str).unwrap_or("?"),
                error.get("error").and_then(Value::as_str).unwrap_or("")
            ));
        }
    }
    Ok((manager, state))
}

// ---------------------------------------------------------------- host requests

async fn host_call(method: &str, params: Value, app: &Arc<Mutex<Option<Weak<crate::app::App>>>>) -> Result<Value> {
    let attached = || app.lock().unwrap().as_ref().and_then(Weak::upgrade);
    match method {
        "log" => {
            let level = params.get("level").and_then(Value::as_str).unwrap_or("info");
            let message = params.get("message").and_then(Value::as_str).unwrap_or("");
            crate::util::log(&format!("[plugin:{level}] {message}"));
            Ok(Value::Null)
        }
        "shell.exec" => {
            let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let mut cmd = if let Some(argv) = params.get("argv").and_then(Value::as_array) {
                let mut args = argv.iter().filter_map(Value::as_str);
                let program = args.next().ok_or_else(|| anyhow!("shell.exec argv is empty"))?;
                let mut cmd = tokio::process::Command::new(program);
                cmd.args(args);
                cmd
            } else {
                let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                let kind = crate::tools::shell::detect();
                let mut cmd = tokio::process::Command::new(&kind.program);
                cmd.args(&kind.args).arg(command);
                cmd
            };
            cmd.current_dir(cwd).stdin(std::process::Stdio::null()).kill_on_drop(true);
            #[cfg(windows)]
            {
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            if let Some(Value::Object(env)) = params.get("env") {
                for (key, value) in env {
                    if let Some(value) = value.as_str() {
                        cmd.env(key, value);
                    }
                }
            }
            let output = cmd.output().await?;
            Ok(json!({
                "stdout": crate::util::decode_output(&output.stdout),
                "stderr": crate::util::decode_output(&output.stderr),
                "exitCode": output.status.code().unwrap_or(-1),
            }))
        }
        "auth.set" => {
            let provider = params
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("auth.set requires provider"))?;
            if let Some(key) = params.get("key").and_then(Value::as_str) {
                let metadata = params.get("metadata").and_then(Value::as_object).map(|values| {
                    values
                        .iter()
                        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                        .collect()
                });
                crate::auth::set(provider, crate::auth::Credential::Api { key: key.to_string(), metadata })?;
            } else if let Some(oauth) = params.get("oauth") {
                crate::auth::set(
                    provider,
                    crate::auth::Credential::Oauth {
                        refresh: oauth.get("refresh").and_then(Value::as_str).unwrap_or("").to_string(),
                        access: oauth.get("access").and_then(Value::as_str).unwrap_or("").to_string(),
                        expires: oauth.get("expires").and_then(Value::as_i64).unwrap_or(0),
                        account_id: oauth.get("accountId").and_then(Value::as_str).map(String::from),
                        enterprise_url: oauth.get("enterpriseUrl").and_then(Value::as_str).map(String::from),
                        metadata: oauth.get("metadata").cloned().filter(|value| !value.is_null()),
                    },
                )?;
            }
            Ok(Value::Null)
        }
        "permission.assert" => {
            let Some(app) = attached() else {
                return Err(anyhow!("application is not ready"));
            };
            let session_id = params.get("sessionID").and_then(Value::as_str).unwrap_or_default();
            let handle = app.session(session_id)?;
            let session_agent = handle.with(|session| session.agent.clone());
            let agent_id = params
                .get("agent")
                .and_then(Value::as_str)
                .map(String::from)
                .or(session_agent)
                .unwrap_or_else(|| app.default_agent());
            let agent = app.agent(&agent_id).or_else(|_| app.agent(&app.default_agent()))?;
            let action = params.get("action").and_then(Value::as_str).unwrap_or("tool");
            let resources: Vec<String> = params
                .get("resources")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            if resources.is_empty() {
                return Err(anyhow!("permission.assert requires at least one resource"));
            }
            let save: Vec<String> = params
                .get("save")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_else(|| resources.clone());
            let metadata = params.get("metadata").cloned().unwrap_or(Value::Null);
            let title = format!("Allow {action} for {}", resources.join(", "));
            app.authorize(&handle, &agent, None, action, &resources, &save, title, metadata).await?;
            Ok(Value::Null)
        }
        "websearch.builtin" => {
            let provider = params.get("providerID").and_then(Value::as_str);
            let query = params.get("query").and_then(Value::as_str).unwrap_or_default();
            let (provider_id, results) = crate::tools::websearch::execute_builtin(provider, query).await?;
            Ok(json!({ "providerID": provider_id, "results": results }))
        }
        "client.call" => {
            let Some(app) = attached() else {
                return Err(anyhow!("application is not ready"));
            };
            let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
            client_call(&app, path, args).await
        }
        _ => Err(anyhow!("unknown host request: {method}")),
    }
}

async fn plugin_generate(
    app: &Arc<crate::app::App>,
    session_id: &str,
    agent_id: Option<String>,
    session_model: Option<String>,
    prompt: String,
    model: Option<Value>,
    contextful: bool,
) -> Result<Value> {
    let agent_id = agent_id.unwrap_or_else(|| app.default_agent());
    let agent = app.agent(&agent_id).or_else(|_| app.agent(&app.default_agent()))?;
    let (model_ref, variant) = match model.as_ref() {
        Some(model) if model.is_string() => (model.as_str().map(String::from), None),
        Some(model) => {
            let reference = match (
                model.get("providerID").and_then(Value::as_str),
                model.get("id").or_else(|| model.get("modelID")).and_then(Value::as_str),
            ) {
                (Some(provider_id), Some(id)) => Some(format!("{provider_id}/{id}")),
                _ => None,
            };
            (reference, model.get("variant").and_then(Value::as_str).map(String::from))
        }
        None => (None, None),
    };
    let opts = crate::app::PromptOptions { model: model_ref, variant, ..Default::default() };
    let mut sel = crate::engine::select_model(app, &agent, &opts, session_model.as_deref())?;
    if contextful {
        crate::engine::resolve_model_hook(app, &agent, &mut sel, session_id, "generate").await;
    }
    let mut system: Vec<String> = Vec::new();
    let mut messages;
    let mut specs: Vec<crate::provider::ToolSpec> = Vec::new();
    if contextful {
        let handle = app.session(session_id)?;
        let registry = crate::tools::registry(app, &agent, &sel.model).await;
        let names: Vec<String> = registry.iter().map(|tool| tool.name().to_string()).collect();
        system = crate::prompt::system(app, &agent, &sel.model, &names).await;
        let entries: Vec<String> = handle.with(|session| {
            session
                .instruction_entries
                .values()
                .filter_map(|value| {
                    value
                        .as_str()
                        .map(String::from)
                        .or_else(|| value.get("text").and_then(Value::as_str).map(String::from))
                })
                .collect()
        });
        system.extend(entries);
        messages = handle.with(|session| session.active_messages().to_vec());
        messages.push(crate::session::Message::user_text(prompt));
        specs = registry.iter().map(|tool| crate::tools::spec(tool.as_ref())).collect();
    } else {
        messages = vec![crate::session::Message::user_text(prompt)];
    }
    let event = if contextful {
        app.plugins
            .trigger(
                "session",
                "generate",
                json!({
                    "sessionID": session_id,
                    "agent": agent.id,
                    "model": crate::engine::model_hook_json(&sel),
                    "system": system.iter().map(|text| json!({ "type": "text", "text": text })).collect::<Vec<_>>(),
                    "messages": messages_for_hook(session_id, &messages),
                    "tools": specs.iter().map(|spec| (spec.name.clone(), json!({
                        "description": spec.description,
                        "input": spec.schema,
                    }))).collect::<serde_json::Map<String, Value>>(),
                    "options": {
                        "maxTokens": crate::provider::max_output(&sel.model),
                        "maxOutputTokens": crate::provider::max_output(&sel.model),
                    },
                }),
            )
            .await
    } else {
        Value::Null
    };
    if let Some(list) = event.get("system").and_then(Value::as_array) {
        system = list
            .iter()
            .filter_map(|item| item.as_str().map(String::from).or_else(|| item.get("text").and_then(Value::as_str).map(String::from)))
            .collect();
    }
    if let Some(list) = event.get("messages") {
        apply_messages_hook(&mut messages, list);
    }
    let session = crate::provider::SessionRef { id: session_id.to_string(), parent_id: None, project: app.project.clone() };
    let mut req = crate::engine::build_request(&sel, &agent, system, messages, specs, &session);
    crate::engine::attach_hooks(app, &mut req, session_id, &agent, &sel, "generate");
    crate::engine::apply_aisdk_hooks(app, &mut req, &sel).await;
    if let Some(options) = event.get("options") {
        if let Some(temperature) = options.get("temperature").and_then(Value::as_f64) {
            req.temperature = Some(temperature);
        }
        if let Some(top_p) = options.get("topP").and_then(Value::as_f64) {
            req.top_p = Some(top_p);
        }
        if let Some(max) = options
            .get("maxTokens")
            .and_then(Value::as_u64)
            .or_else(|| options.get("maxOutputTokens").and_then(Value::as_u64))
        {
            req.max_tokens = max;
        }
        if let Some(extra) = options.get("options").filter(|value| value.is_object()) {
            req.options = extra.clone();
        }
    }
    let (text, usage) = crate::engine::complete_request(req, None).await?;
    let cost = crate::engine::cost(&sel.model, &usage);
    app.plugins.emit(json!({
        "id": crate::util::id("evt"),
        "type": "session.usage.recorded",
        "properties": {
            "sessionID": session_id,
            "source": "generate",
            "cost": cost,
            "tokens": {
                "input": usage.input,
                "output": usage.output,
                "reasoning": usage.reasoning,
                "cache": { "read": usage.cache_read, "write": usage.cache_write },
            },
        },
    }));
    Ok(json!({
        "text": text,
        "cost": crate::engine::cost(&sel.model, &usage),
        "tokens": {
            "input": usage.input,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "cache": { "read": usage.cache_read, "write": usage.cache_write },
        },
    }))
}

fn prompt_files(body: &Value) -> Vec<crate::session::FilePart> {
    let mut files = Vec::new();
    for attachment in body.get("files").and_then(Value::as_array).into_iter().flatten() {
        let name = attachment.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        if let Some(data) = attachment.get("data").and_then(Value::as_str) {
            let mime = attachment
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            files.push(crate::session::FilePart { mime: mime.to_string(), data: data.to_string(), name });
            continue;
        }
        let uri = attachment.get("uri").and_then(Value::as_str).unwrap_or_default();
        if let Some((meta, data)) = uri.split_once(";base64,") {
            files.push(crate::session::FilePart {
                mime: meta.trim_start_matches("data:").to_string(),
                data: data.to_string(),
                name,
            });
        }
    }
    files
}

fn prompt_attachments(skills: &[crate::tools::skill::Skill], body: &Value) -> String {
    let mut parts = Vec::new();
    for agent in body.get("agents").and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = agent.get("name").and_then(Value::as_str) {
            parts.push(format!("[Agent mention: {name}]"));
        }
    }
    for skill in body.get("skills").and_then(Value::as_array).into_iter().flatten() {
        let id = skill.get("id").and_then(Value::as_str);
        let name = skill.get("name").and_then(Value::as_str);
        if let Some(text) = skill.get("text").and_then(Value::as_str) {
            parts.push(format!("[Skill activated: {}]\n{text}", name.unwrap_or("")));
            continue;
        }
        if let Some(found) = skills.iter().find(|candidate| {
            id.is_some_and(|id| candidate.id == id) || name.is_some_and(|name| candidate.name == name)
        }) {
            parts.push(format!("[Skill activated: {}]\n{}", found.name, found.content));
        }
    }
    parts.join("\n\n")
}

fn instruction_hash(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn form_info_json(form: &crate::forms::Form) -> Value {
    let mut value = json!({
        "id": form.id,
        "sessionID": form.session_id,
        "title": form.title,
        "fields": form.fields,
    });
    if !form.metadata.is_null() {
        value["metadata"] = form.metadata.clone();
    }
    value
}

fn form_detail_json(form: &crate::forms::Form) -> Value {
    let mut value = form_info_json(form);
    value["state"] = match form.status {
        crate::forms::Status::Pending => json!({ "status": "pending" }),
        crate::forms::Status::Answered => json!({ "status": "answered", "answer": form.replies }),
        crate::forms::Status::Cancelled => {
            let mut state = json!({ "status": "cancelled" });
            if let Some(message) = &form.message {
                state["message"] = json!(message);
            }
            state
        }
    };
    value
}

fn file_diffs(messages: &[crate::session::Message]) -> Vec<Value> {
    let mut files: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for message in messages {
        for part in &message.parts {
            let crate::session::Part::Tool {
                state: crate::session::ToolState::Completed { metadata, .. },
                ..
            } = part
            else {
                continue;
            };
            let Some(entries) = metadata.get("files").and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(file) = entry.get("file").and_then(Value::as_str) else {
                    continue;
                };
                let diff = entry.get("diff").and_then(Value::as_str).unwrap_or("");
                let status = entry.get("status").and_then(Value::as_str).unwrap_or_else(|| {
                    if diff.contains("--- /dev/null") {
                        "added"
                    } else if diff.contains("+++ /dev/null") {
                        "deleted"
                    } else {
                        "modified"
                    }
                });
                files.insert(
                    file.to_string(),
                    json!({
                        "file": file,
                        "patch": diff,
                        "additions": entry.get("additions").and_then(Value::as_u64).unwrap_or(0),
                        "deletions": entry.get("deletions").and_then(Value::as_u64).unwrap_or(0),
                        "status": status,
                    }),
                );
            }
        }
    }
    files.into_values().collect()
}

fn generate_ref(input: &Value) -> Option<String> {
    let model = input.get("model")?;
    if let Some(reference) = model.as_str() {
        return Some(reference.to_string());
    }
    let provider = model.get("providerID").and_then(Value::as_str)?;
    let id = model
        .get("id")
        .or_else(|| model.get("modelID"))
        .and_then(Value::as_str)?;
    let mut reference = format!("{provider}/{id}");
    if let Some(variant) = model.get("variant").and_then(Value::as_str) {
        reference.push('#');
        reference.push_str(variant);
    }
    Some(reference)
}

pub async fn client_call(app: &Arc<crate::app::App>, path: &str, args: Vec<Value>) -> Result<Value> {
    let first = args.first().cloned().unwrap_or(Value::Null);
    let arg = |key: &str| first.get(key).and_then(Value::as_str).map(String::from);
    // V1 SDK calls use `{ path: { id }, body: { ... } }`; V2 calls are flat.
    let body = first.get("body").unwrap_or(&first).clone();
    let session_id = || -> Option<String> {
        arg("sessionID")
            .or_else(|| arg("id"))
            .or_else(|| first.pointer("/path/id").and_then(Value::as_str).map(String::from))
            .or_else(|| first.pointer("/path/sessionID").and_then(Value::as_str).map(String::from))
    };
    match path {
        "state.snapshot" => {
            let mut value = snapshot(
                &app.config,
                &app.providers(),
                &app.agents(),
                &app.skills(),
                &app.commands(),
                &app.directory,
                &app.worktree,
                &app.project,
            );
            value["defaultAgent"] = json!(app.default_agent());
            value["defaultModel"] = app
                .default_model()
                .as_deref()
                .and_then(crate::models::parse_ref)
                .map(|(provider_id, id, variant)| json!({ "providerID": provider_id, "id": id, "variant": variant }))
                .unwrap_or(Value::Null);
            Ok(value)
        }
        "app.get" | "path.get" => {
            let paths = crate::paths::get();
            Ok(json!({
                "path": {
                    "state": paths.state.to_string_lossy(),
                    "config": paths.config.to_string_lossy(),
                    "worktree": app.worktree.to_string_lossy(),
                    "directory": app.directory.to_string_lossy(),
                },
                "time": { "initialized": 0 },
            }))
        }
        "app.init" => Ok(json!(true)),
        "tui.dialog" => {
            let kind = first.get("type").and_then(Value::as_str).unwrap_or("alert");
            let title = first
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| first.get("message").and_then(Value::as_str))
                .unwrap_or("Plugin");
            let value = first.get("value").cloned().unwrap_or(Value::Null);
            let fields = match kind {
                "confirm" | "alert" => json!([{
                    "key": "value",
                    "title": "OK",
                    "type": "boolean",
                    "default": value.as_bool().unwrap_or(true),
                }]),
                "select" => {
                    let options = first
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    json!([{
                        "key": "value",
                        "title": if options.is_empty() { "Value".to_string() } else { format!("Value (one of: {})", options.join(", ")) },
                        "type": "string",
                        "default": value.as_str().unwrap_or_default(),
                    }])
                }
                _ => json!([{
                    "key": "value",
                    "title": "Value",
                    "type": "string",
                    "default": value.as_str().unwrap_or_default(),
                }]),
            };
            let form = app.forms.create("tui", title, json!({ "kind": kind }), fields);
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "form.created",
                "properties": { "form": form_info_json(&form) }
            }));
            let outcome = app.forms.wait(&form.id).await?;
            Ok(match outcome {
                crate::forms::Outcome::Replied(replies) => {
                    json!({ "value": replies.get("value").cloned().unwrap_or(Value::Null) })
                }
                crate::forms::Outcome::Cancelled => Value::Null,
            })
        }
        "tui.toast" => {
            let message = first
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let title = first.get("title").and_then(Value::as_str).unwrap_or_default();
            let text = if title.is_empty() { message } else { format!("{title}: {message}") };
            if !text.is_empty() {
                app.emit(crate::app::AppEvent::Notice { message: text });
            }
            Ok(Value::Null)
        }
        "project.current" | "project.list" => {
            let project = json!({
                "id": app.project,
                "worktree": app.worktree.to_string_lossy(),
                "vcs": "git",
                "time": { "created": 0, "initialized": 0 },
                "sandboxes": [],
            });
            if path == "project.list" {
                return Ok(json!([project]));
            }
            Ok(project)
        }
        "tui.appendPrompt" | "tui.openHelp" | "tui.openSessions" | "tui.openThemes"
        | "tui.openModels" | "tui.submitPrompt" | "tui.clearPrompt" | "tui.executeCommand"
        | "tui.openDialog" | "tui.control" => Ok(Value::Null),
        "app.log" => {
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| first.get("message").and_then(Value::as_str))
                .unwrap_or("");
            crate::util::log(&format!("[plugin] {message}"));
            Ok(Value::Null)
        }
        "tui.showToast" => {
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| first.get("message").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            app.emit(crate::app::AppEvent::Notice { message });
            Ok(Value::Null)
        }
        "config.get" => Ok(serde_json::to_value(&app.config)?),
        "session.list" => {
            let mut list = crate::session::list(&app.project);
            let filter = first.get("query").unwrap_or(&first);
            if let Some(parent) = filter.get("parentID") {
                if parent.is_null() {
                    list.retain(|s| s.parent_id.is_none());
                } else if let Some(parent) = parent.as_str() {
                    list.retain(|s| s.parent_id.as_deref() == Some(parent));
                }
            }
            if let Some(search) = filter.get("search").and_then(Value::as_str) {
                let search = search.to_lowercase();
                list.retain(|s| s.title.to_lowercase().contains(&search));
            }
            if let Some(directory) = filter.get("directory").and_then(Value::as_str) {
                list.retain(|s| s.directory.to_string_lossy() == directory);
            }
            if let Some(directory) = first
                .pointer("/location/directory")
                .and_then(Value::as_str)
                .or_else(|| first.pointer("/path/directory").and_then(Value::as_str))
            {
                list.retain(|s| s.directory.to_string_lossy() == directory);
            }
            if let Some(project) = filter.get("project").and_then(Value::as_str) {
                list.retain(|s| crate::session::project_id(&s.directory) == project);
            }
            match filter.get("archived").and_then(Value::as_bool) {
                Some(true) => list.retain(|s| s.archived),
                Some(false) => list.retain(|s| !s.archived),
                None => {}
            }
            if filter.get("order").and_then(Value::as_str) == Some("asc") {
                list.reverse();
            }
            if let Some(cursor) = filter.get("cursor").and_then(Value::as_str) {
                if let Some(index) = list.iter().position(|session| session.id == cursor) {
                    list.drain(..=index);
                }
            }
            let previous_cursor = filter.get("cursor").and_then(Value::as_str).map(String::from);
            let has_more = filter.get("limit").and_then(Value::as_u64).is_some_and(|limit| list.len() > limit as usize);
            if let Some(limit) = filter.get("limit").and_then(Value::as_u64) {
                list.truncate(limit as usize);
            }
            Ok(json!({
                "data": list.iter().map(session_json_summary).collect::<Vec<_>>(),
                "cursor": {
                    "previous": list.first().map(|session| session.id.clone()).or(previous_cursor),
                    "next": if has_more { list.last().map(|session| session.id.clone()) } else { None },
                },
            }))
        }
        "session.get" => {
            let id = session_id().ok_or_else(|| anyhow!("session.get requires sessionID"))?;
            let handle = app.session(&id)?;
            let session = handle.with(|s| s.clone());
            Ok(session_json(&session))
        }
        "session.fork" => {
            let id = session_id().ok_or_else(|| anyhow!("session.fork requires sessionID"))?;
            let handle = app.session(&id)?;
            let before = body.get("before").and_then(Value::as_str).map(String::from);
            let mut forked = {
                let session = handle.session.lock().unwrap();
                let mut forked = crate::session::fork(&session);
                if let Some(before) = &before {
                    if let Some(index) = forked.messages.iter().position(|message| message.id == *before) {
                        forked.messages.truncate(index);
                    }
                }
                forked
            };
            let boundary = match &before {
                Some(message_id) => json!({ "type": "before", "messageID": message_id }),
                None => forked
                    .messages
                    .last()
                    .map(|message| json!({ "type": "through", "messageID": message.id }))
                    .unwrap_or_else(|| json!({ "type": "before", "messageID": "" })),
            };
            forked.fork = Some(json!({ "sessionID": id, "boundary": boundary.clone() }));
            crate::session::save(&forked)?;
            let forked = app.session(&forked.id)?;
            let session = forked.with(|session| session.clone());
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.forked",
                "properties": {
                    "sessionID": session.id,
                    "parentID": id,
                    "boundary": boundary.clone(),
                }
            }));
            let mut value = session_json(&session);
            value["fork"] = json!({ "sessionID": id, "boundary": boundary });
            Ok(value)
        }
        "session.background" => Ok(Value::Null),
        "session.share" | "session.unshare" => {
            let id = session_id().ok_or_else(|| anyhow!("session.share requires sessionID"))?;
            let handle = app.session(&id)?;
            let session = handle.with(|session| session.clone());
            Ok(session_json(&session))
        }
        "session.shell" => {
            let id = session_id().ok_or_else(|| anyhow!("session.shell requires sessionID"))?;
            let command = body.get("command").and_then(Value::as_str).unwrap_or("").to_string();
            let handle = app.session(&id)?;
            let (cwd, environment) = handle.with(|session| (session.directory.clone(), session.environment.clone()));
            let shell = crate::tools::shell::detect();
            let shell_id = body
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| id.starts_with("sh_"))
                .map(String::from)
                .unwrap_or_else(|| crate::util::id("sh"));
            let file = crate::paths::get().tool_output().join(format!("{shell_id}.log"));
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new(".")));
            let started = crate::util::now_ms();
            let info = json!({
                "id": shell_id.clone(),
                "status": "running",
                "command": command.clone(),
                "cwd": cwd.to_string_lossy(),
                "shell": shell.program.to_string_lossy(),
                "file": file.to_string_lossy(),
                "metadata": { "sessionID": id, "background": true },
                "time": { "started": started },
            });
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.shell.started",
                "properties": { "sessionID": id.clone(), "shell": info.clone() }
            }));
            handle.with(|session| {
                session.shells.push(crate::session::ShellMessage {
                    id: shell_id.clone(),
                    command: command.clone(),
                    status: "running".into(),
                    exit: None,
                    output: None,
                    started,
                    completed: None,
                });
            });
            handle.save();
            let app = app.clone();
            let session_id = id.clone();
            let shell_id = info["id"].as_str().unwrap_or_default().to_string();
            let shell_program = shell.program.to_string_lossy().into_owned();
            tokio::spawn(async move {
                let mut cmd = tokio::process::Command::new(&shell.program);
                cmd.args(&shell.args).arg(&command).current_dir(&cwd).stdin(std::process::Stdio::null()).kill_on_drop(true);
                for (key, value) in &environment {
                    cmd.env(key, value);
                }
                #[cfg(windows)]
                {
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    cmd.creation_flags(CREATE_NO_WINDOW);
                }
                let (status, exit, text) = match cmd.output().await {
                    Ok(output) => (
                        "exited",
                        output.status.code().unwrap_or(-1),
                        format!(
                            "{}{}",
                            crate::util::decode_output(&output.stdout),
                            crate::util::decode_output(&output.stderr)
                        ),
                    ),
                    Err(error) => ("killed", -1, error.to_string()),
                };
                let _ = std::fs::write(&file, &text);
                let completed = crate::util::now_ms();
                let size = text.len();
                if let Ok(handle) = app.session(&session_id) {
                    handle.with(|session| {
                        if let Some(shell) = session.shells.iter_mut().find(|shell| shell.id == shell_id) {
                            shell.status = status.to_string();
                            shell.exit = Some(exit as i64);
                            shell.output = Some(text.clone());
                            shell.completed = Some(completed);
                        }
                    });
                    handle.save();
                }
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.shell.ended",
                    "properties": {
                        "sessionID": session_id.clone(),
                        "shell": {
                            "id": shell_id,
                            "status": status,
                            "command": command,
                            "cwd": cwd.to_string_lossy(),
                            "shell": shell_program,
                            "file": file.to_string_lossy(),
                            "exit": exit,
                            "metadata": { "sessionID": session_id, "background": true },
                            "time": { "started": started, "completed": completed },
                        },
                        "output": {
                            "output": text,
                            "cursor": size,
                            "size": size,
                            "truncated": false,
                        }
                    }
                }));
            });
            Ok(Value::Null)
        }
        "session.skill" => {
            let session_id = session_id().ok_or_else(|| anyhow!("session.skill requires sessionID"))?;
            let skill_id = body
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("session.skill requires a skill id"))?;
            let skill = app
                .skills()
                .into_iter()
                .find(|skill| skill.id == skill_id)
                .ok_or_else(|| anyhow!("skill not found: {skill_id}"))?;
            let text = format!("[Skill activated: {}]\n{}", skill.name, skill.content);
            let handle = app.session(&session_id)?;
            handle.with(|session| {
                session
                    .instruction_entries
                    .insert(format!("skill.{}", skill.id), json!({ "name": skill.name, "text": text.clone() }));
                session.record(json!({
                    "type": "skill",
                    "skill": skill.id,
                    "name": skill.name,
                    "text": text.clone(),
                }));
            });
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.instructions.updated",
                "properties": {
                    "sessionID": session_id.clone(),
                    "delta": { (format!("api/skill.{}", skill.id)): instruction_hash(&json!(text.clone())) }
                }
            }));
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.skill.activated",
                "properties": {
                    "sessionID": session_id,
                    "id": skill.id,
                    "name": skill.name,
                    "text": text,
                }
            }));
            Ok(Value::Null)
        }
        "session.create" | "session.init" => {
            let parent = body.get("parentID").and_then(Value::as_str).map(String::from);
            let explicit = body.get("id").and_then(Value::as_str).map(String::from);
            let (handle, created) = match explicit {
                Some(id) => match app.session(&id) {
                    Ok(handle) => (handle, false),
                    Err(_) => {
                        let handle = app.new_session(parent);
                        handle.with(|session| session.id = id);
                        (handle, true)
                    }
                },
                None => (app.new_session(parent), true),
            };
            handle.with(|session| {
                if let Some(directory) = body.pointer("/location/directory").and_then(Value::as_str) {
                    let directory = std::path::PathBuf::from(directory);
                    session.directory = directory.clone();
                    session.project = crate::session::project_id(&directory);
                }
                if let Some(title) = body.get("title").and_then(Value::as_str) {
                    session.title = title.to_string();
                }
                if let Some(agent) = body.get("agent").and_then(Value::as_str) {
                    session.agent = Some(agent.to_string());
                }
                if let Some(metadata) = body.get("metadata") {
                    session.metadata = Some(metadata.clone());
                }
                if let Some(permissions) = body.get("permissions") {
                    session.permissions = Some(permissions.clone());
                }
                if let Some(archived) = body.get("archived").and_then(Value::as_bool) {
                    session.archived = archived;
                }
                if let Some(model) = body.get("model") {
                    if let (Some(provider_id), Some(id)) = (
                        model.get("providerID").and_then(Value::as_str),
                        model.get("id").or_else(|| model.get("modelID")).and_then(Value::as_str),
                    ) {
                        let mut reference = format!("{provider_id}/{id}");
                        if let Some(variant) = model.get("variant").and_then(Value::as_str) {
                            reference.push('#');
                            reference.push_str(variant);
                        }
                        session.model = Some(reference);
                    }
                }
            });
            handle.save();
            let session = handle.with(|s| s.clone());
            if created {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.created",
                    "properties": { "info": session_json(&session) }
                }));
            }
            if let Some(title) = body.get("title").and_then(Value::as_str) {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.renamed",
                    "properties": { "sessionID": session.id, "title": title }
                }));
            }
            if let Some(agent) = body.get("agent").and_then(Value::as_str) {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.agent.selected",
                    "properties": { "sessionID": session.id, "agent": agent }
                }));
            }
            if let Some(model) = session.model.as_deref().and_then(crate::models::parse_ref) {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.model.selected",
                    "properties": {
                        "sessionID": session.id,
                        "model": { "providerID": model.0, "id": model.1, "variant": model.2 }
                    }
                }));
            }
            if let Some(permissions) = body.get("permissions") {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.permissions",
                    "properties": { "sessionID": session.id, "permissions": permissions }
                }));
            }
            Ok(session_json(&session))
        }
        "session.remove" | "session.delete" => {
            let id = session_id().ok_or_else(|| anyhow!("session.remove requires sessionID"))?;
            app.session(&id)?;
            pty::manager().kill(&id);
            crate::provider::ws_transport::close(&id).await;
            crate::session::delete(&app.project, &id)?;
            app.forget_session(&id);
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.deleted",
                "properties": { "sessionID": id }
            }));
            Ok(json!(true))
        }
        "session.update" => {
            let id = session_id().ok_or_else(|| anyhow!("session.update requires sessionID"))?;
            let handle = app.session(&id)?;
            handle.with(|session| {
                if let Some(title) = body.get("title").and_then(Value::as_str) {
                    session.title = title.to_string();
                }
                if let Some(metadata) = body.get("metadata") {
                    session.metadata = Some(metadata.clone());
                }
                if let Some(permissions) = body.get("permissions") {
                    session.permissions = Some(permissions.clone());
                }
                if let Some(archived) = body.get("archived").and_then(Value::as_bool) {
                    session.archived = archived;
                }
            });
            handle.save();
            if let Some(title) = body.get("title").and_then(Value::as_str) {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.renamed",
                    "properties": { "sessionID": id, "title": title }
                }));
            }
            if let Some(metadata) = body.get("metadata") {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.metadata.updated",
                    "properties": { "sessionID": id, "metadata": metadata }
                }));
            }
            if let Some(permissions) = body.get("permissions") {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.permissions",
                    "properties": { "sessionID": id, "permissions": permissions }
                }));
            }
            if let Some(archived) = body.get("archived").and_then(Value::as_bool) {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": if archived { "session.archived" } else { "session.unarchived" },
                    "properties": { "sessionID": id }
                }));
            }
            Ok(Value::Null)
        }
        "session.messages" => {
            let id = session_id().ok_or_else(|| anyhow!("session.messages requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|s| {
                let mut values = session_history_json(&s.messages, &s.shells, &s.records)
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if let Some(kind) = first.get("type").and_then(Value::as_str) {
                    values.retain(|value| value.get("type").and_then(Value::as_str) == Some(kind));
                }
                if first.get("order").and_then(Value::as_str) == Some("desc") {
                    values.reverse();
                }
                if let Some(cursor) = first.get("cursor").and_then(Value::as_str) {
                    if let Some(index) = values.iter().position(|value| value.get("id").and_then(Value::as_str) == Some(cursor)) {
                        values.drain(..=index);
                    }
                }
                let has_more = first.get("limit").and_then(Value::as_u64).is_some_and(|limit| values.len() > limit as usize);
                if let Some(limit) = first.get("limit").and_then(Value::as_u64) {
                    values.truncate(limit as usize);
                }
                let id = |value: &Value| value.get("id").and_then(Value::as_str).map(String::from);
                json!({
                    "data": Value::Array(values.clone()),
                    "cursor": {
                        "previous": values.first().and_then(id),
                        "next": if has_more { values.last().and_then(id) } else { None },
                    },
                })
            }))
        }
        "session.status" => {
            let Some(id) = session_id() else {
                let mut statuses = Map::new();
                for (id, busy, retry) in app.session_statuses() {
                    statuses.insert(
                        id,
                        retry.unwrap_or_else(|| json!({ "type": if busy { "busy" } else { "idle" } })),
                    );
                }
                return Ok(Value::Object(statuses));
            };
            let handle = app.session(&id)?;
            if let Some(retry) = handle.retry.lock().unwrap().clone() {
                return Ok(retry);
            }
            Ok(json!({ "type": if handle.is_busy() { "busy" } else { "idle" } }))
        }
        "session.active" => {
            let mut active = Map::new();
            for id in app.active_session_ids() {
                active.insert(id, json!({ "type": "running" }));
            }
            Ok(Value::Object(active))
        }
        "session.wait" | "experimental.session.wait" => {
            let id = session_id().ok_or_else(|| anyhow!("session.wait requires sessionID"))?;
            let handle = app.session(&id)?;
            while handle.is_busy() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(Value::Null)
        }
        "session.log" | "experimental.session.log" => {
            let id = session_id().ok_or_else(|| anyhow!("session.log requires sessionID"))?;
            let after = body.get("after").and_then(Value::as_u64);
            let limit = body.get("limit").and_then(Value::as_u64).map(|limit| limit as usize);
            Ok(json!({
                "data": crate::event_store::read(Some(&id), after, limit),
            }))
        }
        "session.diff" => {
            let id = session_id().ok_or_else(|| anyhow!("session.diff requires sessionID"))?;
            let handle = app.session(&id)?;
            let from = arg("from");
            let to = arg("to");
            let snapshots = handle.with(|session| {
                let snapshot = |id: Option<&str>| {
                    id.and_then(|id| session.messages.iter().find(|message| message.id == id))
                        .or_else(|| session.messages.iter().rev().find(|message| message.role == crate::session::Role::User))
                        .and_then(|message| message.snapshot.clone())
                };
                (snapshot(from.as_deref()), snapshot(to.as_deref()))
            });
            if let Some(from) = snapshots.0 {
                if let Ok(diffs) = crate::snapshot::diff(&app.worktree, &from, snapshots.1.as_deref()) {
                    return Ok(json!(diffs));
                }
            }
            let messages = handle.with(|session| {
                let mut start = session
                    .messages
                    .iter()
                    .rposition(|message| message.role == crate::session::Role::User)
                    .unwrap_or(0);
                let mut end = session.messages.len();
                if let Some(from) = from.as_deref() {
                    if let Some(index) = session.messages.iter().position(|message| message.id == from) {
                        start = index + 1;
                    }
                }
                if let Some(to) = to.as_deref() {
                    if let Some(index) = session.messages.iter().position(|message| message.id == to) {
                        end = index + 1;
                    }
                }
                if start >= end {
                    Vec::new()
                } else {
                    session.messages[start..end].to_vec()
                }
            });
            Ok(json!(file_diffs(&messages)))
        }
        "session.environment" => {
            let id = session_id().ok_or_else(|| anyhow!("session.environment requires sessionID"))?;
            let handle = app.session(&id)?;
            if let Some(variables) = body.get("variables").and_then(Value::as_object) {
                handle.with(|session| {
                    session.environment = variables
                        .iter()
                        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                        .collect();
                });
                pty::manager().kill(&id);
                handle.save();
            }
            Ok(Value::Null)
        }
        "session.view" => {
            let id = session_id().ok_or_else(|| anyhow!("session.view requires sessionID"))?;
            let handle = app.session(&id)?;
            let Some(idle) = first.get("idle").and_then(Value::as_i64) else {
                return Ok(Value::Null);
            };
            let mut changed = false;
            handle.with(|session| {
                if session.idle_ms.is_none_or(|current| idle >= current) {
                    session.viewed_at = Some(crate::util::now_ms());
                    session.idle_ms = Some(idle);
                    changed = true;
                }
            });
            if changed {
                handle.save();
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.viewed",
                    "properties": { "sessionID": id, "idle": idle }
                }));
            }
            Ok(Value::Null)
        }
        "session.form.list" => {
            let id = session_id().ok_or_else(|| anyhow!("session.form.list requires sessionID"))?;
            app.session(&id)?;
            Ok(json!(app.forms.list(&id).iter().map(form_info_json).collect::<Vec<_>>()))
        }
        "session.form.create" => {
            let id = session_id().ok_or_else(|| anyhow!("session.form.create requires sessionID"))?;
            app.session(&id)?;
            let title = body.get("title").and_then(Value::as_str).unwrap_or("Form");
            let metadata = body.get("metadata").cloned().unwrap_or(Value::Null);
            let fields = body.get("fields").cloned().unwrap_or_else(|| json!([]));
            if fields.as_array().is_none_or(|fields| fields.is_empty()) {
                return Err(anyhow!("session.form.create requires at least one field"));
            }
            let requested_id = body.get("id").and_then(Value::as_str);
            if let Some(requested_id) = requested_id {
                if app.forms.get(requested_id).is_ok() {
                    return Err(anyhow!("form already exists: {requested_id}"));
                }
            }
            let form = app.forms.create_with_id(requested_id, &id, title, metadata, fields);
            let info = form_info_json(&form);
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "form.created",
                "properties": { "form": info.clone() }
            }));
            Ok(info)
        }
        "session.form.get" => {
            let id = session_id().ok_or_else(|| anyhow!("session.form.get requires sessionID"))?;
            app.session(&id)?;
            let form_id = arg("formID").ok_or_else(|| anyhow!("session.form.get requires formID"))?;
            let form = app.forms.get(&form_id)?;
            if form.session_id != id {
                return Err(anyhow!("form not found: {form_id}"));
            }
            Ok(form_detail_json(&form))
        }
        "session.form.reply" => {
            let id = session_id().ok_or_else(|| anyhow!("session.form.reply requires sessionID"))?;
            app.session(&id)?;
            let form_id = arg("formID").ok_or_else(|| anyhow!("session.form.reply requires formID"))?;
            let existing = app.forms.get(&form_id)?;
            if existing.session_id != id {
                return Err(anyhow!("form not found: {form_id}"));
            }
            let answer = body.get("answer").cloned().unwrap_or_else(|| json!({}));
            let form = app.forms.reply(&form_id, answer.clone())?;
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "form.replied",
                "properties": { "id": form_id, "sessionID": form.session_id, "answer": answer }
            }));
            Ok(Value::Null)
        }
        "session.form.cancel" => {
            let id = session_id().ok_or_else(|| anyhow!("session.form.cancel requires sessionID"))?;
            app.session(&id)?;
            let form_id = arg("formID").ok_or_else(|| anyhow!("session.form.cancel requires formID"))?;
            let existing = app.forms.get(&form_id)?;
            if existing.session_id != id {
                return Err(anyhow!("form not found: {form_id}"));
            }
            let message = body.get("message").and_then(Value::as_str).map(String::from);
            let form = app.forms.cancel_with_message(&form_id, message)?;
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "form.cancelled",
                "properties": { "id": form_id, "sessionID": form.session_id }
            }));
            Ok(Value::Null)
        }
        "session.revert.stage" | "session.revert" => {
            let id = session_id().ok_or_else(|| anyhow!("session.revert.stage requires sessionID"))?;
            let message_id = arg("messageID").ok_or_else(|| anyhow!("session.revert.stage requires messageID"))?;
            let handle = app.session(&id)?;
            handle.with(|session| {
                if !session.messages.iter().any(|message| message.id == message_id) {
                    return Err(anyhow!("message not found: {message_id}"));
                }
                Ok(())
            })?;
            let snapshot = handle.with(|session| {
                session
                    .messages
                    .iter()
                    .find(|message| message.id == message_id)
                    .and_then(|message| message.snapshot.clone())
            });
            let files = if first.get("files").and_then(Value::as_bool).unwrap_or(true) {
                let snapshot_diff = snapshot
                    .as_deref()
                    .and_then(|snapshot| crate::snapshot::diff(&app.worktree, snapshot, None).ok());
                let messages = handle.with(|session| {
                    session
                        .messages
                        .iter()
                        .position(|message| message.id == message_id)
                        .map(|index| session.messages[index + 1..].to_vec())
                        .unwrap_or_default()
                });
                Some(json!(snapshot_diff.unwrap_or_else(|| file_diffs(&messages))))
            } else {
                None
            };
            let mut revert = json!({ "messageID": message_id });
            if let Some(files) = files {
                revert["files"] = files;
            }
            if let Some(snapshot) = snapshot {
                revert["snapshot"] = json!(snapshot);
            }
            handle.with(|session| session.revert = Some(revert.clone()));
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.revert.staged",
                "properties": { "sessionID": id, "revert": revert }
            }));
            Ok(revert)
        }
        "session.revert.clear" | "session.unrevert" => {
            let id = session_id().ok_or_else(|| anyhow!("session.revert.clear requires sessionID"))?;
            let handle = app.session(&id)?;
            handle.with(|session| session.revert = None);
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.revert.cleared",
                "properties": { "sessionID": id }
            }));
            Ok(Value::Null)
        }
        "session.revert.commit" => {
            let id = session_id().ok_or_else(|| anyhow!("session.revert.commit requires sessionID"))?;
            let handle = app.session(&id)?;
            let Some(revert) = handle.with(|session| session.revert.clone()) else {
                return Ok(Value::Null);
            };
            let Some(message_id) = revert.get("messageID").and_then(Value::as_str).map(String::from) else {
                return Ok(Value::Null);
            };
            if let Some(snapshot) = revert.get("snapshot").and_then(Value::as_str) {
                crate::snapshot::restore(&app.worktree, snapshot)?;
            }
            handle.with(|session| {
                if let Some(index) = session.messages.iter().position(|message| message.id == message_id) {
                    session.messages.truncate(index + 1);
                }
                session.revert = None;
            });
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.revert.committed",
                "properties": { "sessionID": id, "to": message_id }
            }));
            Ok(Value::Null)
        }
        "session.inbox.list" => {
            let id = session_id().ok_or_else(|| anyhow!("session.inbox.list requires sessionID"))?;
            let handle = app.session(&id)?;
            let items: Vec<Value> = handle
                .queue
                .lock()
                .unwrap()
                .iter()
                .map(|(message, options)| {
                    let mut item =
                        crate::engine::inbox_item(message, options.delivery.as_deref().unwrap_or("steer"));
                    if let Some(map) = item.as_object_mut() {
                        map.insert("id".into(), json!(message.id.clone()));
                        map.insert("sessionID".into(), json!(id.clone()));
                        map.insert("time".into(), json!({ "created": message.time }));
                    }
                    item
                })
                .collect();
            Ok(json!(items))
        }
        "session.inbox.cancel" => {
            let id = session_id().ok_or_else(|| anyhow!("session.inbox.cancel requires sessionID"))?;
            let inbox_id = arg("inboxID").ok_or_else(|| anyhow!("session.inbox.cancel requires inboxID"))?;
            let handle = app.session(&id)?;
            let mut queue = handle.queue.lock().unwrap();
            let before = queue.len();
            queue.retain(|(message, _)| message.id != inbox_id);
            let removed = queue.len() != before;
            drop(queue);
            if removed {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.cancelled",
                    "properties": { "sessionID": id, "inboxID": inbox_id }
                }));
            }
            Ok(Value::Null)
        }
        "session.inbox.update" => {
            let id = session_id().ok_or_else(|| anyhow!("session.inbox.update requires sessionID"))?;
            let handle = app.session(&id)?;
            let delivery = body.get("delivery").and_then(Value::as_str).unwrap_or("steer");
            let inbox_id = arg("inboxID").ok_or_else(|| anyhow!("session.inbox.update requires inboxID"))?;
            let mut queue = handle.queue.lock().unwrap();
            let mut updated = false;
            for (message, options) in queue.iter_mut() {
                if message.id == inbox_id {
                    options.delivery = Some(delivery.to_string());
                    updated = true;
                }
            }
            drop(queue);
            if updated {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.delivery.changed",
                    "properties": { "sessionID": id, "inboxID": inbox_id, "delivery": delivery }
                }));
            }
            Ok(Value::Null)
        }
        "session.message.list" => {
            let id = session_id().ok_or_else(|| anyhow!("session.message.list requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|s| {
                let mut values = session_history_json(&s.messages, &s.shells, &s.records)
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if let Some(kind) = first.get("type").and_then(Value::as_str) {
                    values.retain(|value| value.get("type").and_then(Value::as_str) == Some(kind));
                }
                if first.get("order").and_then(Value::as_str) == Some("desc") {
                    values.reverse();
                }
                if let Some(cursor) = first.get("cursor").and_then(Value::as_str) {
                    if let Some(index) = values.iter().position(|value| value.get("id").and_then(Value::as_str) == Some(cursor)) {
                        values.drain(..=index);
                    }
                }
                let has_more = first.get("limit").and_then(Value::as_u64).is_some_and(|limit| values.len() > limit as usize);
                if let Some(limit) = first.get("limit").and_then(Value::as_u64) {
                    values.truncate(limit as usize);
                }
                let id = |value: &Value| value.get("id").and_then(Value::as_str).map(String::from);
                json!({
                    "data": Value::Array(values.clone()),
                    "cursor": {
                        "previous": values.first().and_then(id),
                        "next": if has_more { values.last().and_then(id) } else { None },
                    },
                })
            }))
        }
        "session.message.get" | "session.message" => {
            let id = session_id().ok_or_else(|| anyhow!("session.message.get requires sessionID"))?;
            let message_id = first
                .get("messageID")
                .or_else(|| first.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("session.message.get requires messageID"))?;
            let handle = app.session(&id)?;
            let message = handle.with(|s| s.messages.iter().find(|m| m.id == message_id).cloned());
            match message {
                Some(message) => {
                    let value = session_messages_json(std::slice::from_ref(&message));
                    Ok(value.as_array().and_then(|items| items.first()).cloned().unwrap_or(Value::Null))
                }
                None => {
                    let found = handle.with(|s| {
                        s.shells
                            .iter()
                            .find(|shell| shell.id == message_id)
                            .map(shell_message_json)
                            .or_else(|| {
                                s.records
                                    .iter()
                                    .find(|record| record.get("id").and_then(Value::as_str) == Some(message_id))
                                    .cloned()
                            })
                    });
                    found.ok_or_else(|| anyhow!("message not found: {message_id}"))
                }
            }
        }
        "session.context" => {
            let id = session_id().ok_or_else(|| anyhow!("session.context requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|s| session_history_json(&s.active_messages(), &s.shells, &s.records)))
        }
        "session.instructions" => {
            let id = session_id().ok_or_else(|| anyhow!("session.instructions requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|session| {
                json!(session
                    .instruction_entries
                    .iter()
                    .map(|(key, value)| json!({ "key": key, "value": value }))
                    .collect::<Vec<_>>())
            }))
        }
        "session.instructions.entry.list" => {
            let id = session_id().ok_or_else(|| anyhow!("session.instructions.entry.list requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|session| {
                json!(session
                    .instruction_entries
                    .iter()
                    .filter_map(|(key, value)| {
                        let key = key.strip_prefix("api/")?;
                        Some(json!({ "key": key, "value": value }))
                    })
                    .collect::<Vec<_>>())
            }))
        }
        "session.instructions.entry.put" => {
            let id = session_id().ok_or_else(|| anyhow!("session.instructions.entry.put requires sessionID"))?;
            let handle = app.session(&id)?;
            let key = body.get("key").and_then(Value::as_str).unwrap_or_default().to_string();
            if key.is_empty() || key.contains('/') {
                return Err(anyhow!("instruction entry key must be slash-free"));
            }
            let value = body.get("value").cloned().unwrap_or(Value::Null);
            let hash = instruction_hash(&value);
            let stored = format!("api/{key}");
            handle.with(|session| {
                let text = value
                    .as_str()
                    .map(String::from)
                    .or_else(|| value.get("text").and_then(Value::as_str).map(String::from))
                    .unwrap_or_else(|| value.to_string());
                session.record(json!({
                    "type": "system",
                    "text": text,
                    "description": format!("Instruction updated: {key}"),
                }));
                session.instruction_entries.insert(stored.clone(), value);
            });
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.instructions.updated",
                "properties": { "sessionID": id, "delta": { (stored): hash } }
            }));
            Ok(Value::Null)
        }
        "session.instructions.entry.remove" => {
            let id = session_id().ok_or_else(|| anyhow!("session.instructions.entry.remove requires sessionID"))?;
            let handle = app.session(&id)?;
            let key = body.get("key").and_then(Value::as_str).unwrap_or_default();
            if key.is_empty() || key.contains('/') {
                return Err(anyhow!("instruction entry key must be slash-free"));
            }
            let stored = format!("api/{key}");
            handle.with(|session| {
                session.instruction_entries.remove(&stored);
                session.record(json!({
                    "type": "system",
                    "text": "",
                    "description": format!("Instruction removed: {key}"),
                }));
            });
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.instructions.updated",
                "properties": { "sessionID": id, "delta": { (stored): "removed" } }
            }));
            Ok(Value::Null)
        }
        "session.switchAgent" => {
            let id = session_id().ok_or_else(|| anyhow!("session.switchAgent requires sessionID"))?;
            let handle = app.session(&id)?;
            if let Some(agent) = body.get("agent").and_then(Value::as_str) {
                handle.with(|session| {
                    let previous = session.agent.clone();
                    if previous.as_deref() != Some(agent) {
                        session.record(json!({ "type": "agent-switched", "agent": agent, "previous": previous }));
                    }
                    session.agent = Some(agent.to_string());
                });
                handle.save();
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.agent.selected",
                    "properties": { "sessionID": id, "agent": agent }
                }));
            }
            Ok(Value::Null)
        }
        "session.switchModel" => {
            let id = session_id().ok_or_else(|| anyhow!("session.switchModel requires sessionID"))?;
            let handle = app.session(&id)?;
            if let Some(model) = body.get("model") {
                let reference = if let Some(text) = model.as_str() {
                    Some(text.to_string())
                } else {
                    match (model.get("providerID").and_then(Value::as_str), model.get("id").or_else(|| model.get("modelID")).and_then(Value::as_str)) {
                        (Some(provider_id), Some(model_id)) => {
                            let mut value = format!("{provider_id}/{model_id}");
                            if let Some(variant) = model.get("variant").and_then(Value::as_str) {
                                value.push('#');
                                value.push_str(variant);
                            }
                            Some(value)
                        }
                        _ => None,
                    }
                };
                if let Some(reference) = reference {
                    handle.with(|session| {
                        let previous = session.model.clone();
                        if previous.as_deref() != Some(reference.as_str()) {
                            session.record(json!({
                                "type": "model-switched",
                                "model": crate::models::parse_ref(&reference).map(|(provider_id, id, variant)| {
                                    json!({ "providerID": provider_id, "id": id, "variant": variant })
                                }),
                                "previous": previous.as_deref().and_then(crate::models::parse_ref).map(|(provider_id, id, variant)| {
                                    json!({ "providerID": provider_id, "id": id, "variant": variant })
                                }),
                            }));
                        }
                        session.model = Some(reference.clone());
                    });
                    handle.save();
                    if let Some((provider_id, model_id, variant)) = crate::models::parse_ref(&reference) {
                        app.plugins.emit(json!({
                            "id": crate::util::id("evt"),
                            "type": "session.model.selected",
                            "properties": {
                                "sessionID": id,
                                "model": { "providerID": provider_id, "id": model_id, "variant": variant }
                            }
                        }));
                    }
                }
            }
            Ok(Value::Null)
        }
        "session.generate" => {
            let id = session_id().ok_or_else(|| anyhow!("session.generate requires sessionID"))?;
            let handle = app.session(&id)?;
            let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
            let session_agent = handle.with(|session| session.agent.clone());
            let session_model = handle.with(|session| session.model.clone());
            plugin_generate(app, &id, session_agent, session_model, prompt, None, true).await
        }
        "session.move" => {
            let id = session_id().ok_or_else(|| anyhow!("session.move requires sessionID"))?;
            let directory = body
                .get("directory")
                .or_else(|| first.get("directory"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("session.move requires directory"))?;
            let handle = app.session(&id)?;
            let delivery = body.get("delivery").and_then(Value::as_str).unwrap_or("steer");
            let directory = std::path::PathBuf::from(directory);
            if handle.is_busy() && delivery == "queue" {
                let inbox_id = crate::util::id("msg");
                *handle.pending_move.lock().unwrap() = Some(directory.clone());
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.enqueued",
                    "properties": {
                        "sessionID": id,
                        "inboxID": inbox_id,
                        "item": {
                            "type": "move",
                            "payload": {
                                "projectID": crate::session::project_id(&directory),
                                "location": { "directory": directory.to_string_lossy() }
                            },
                            "delivery": "queue"
                        }
                    }
                }));
                return Ok(Value::Null);
            }
            pty::manager().kill(&id);
            crate::provider::ws_transport::close(&id).await;
            handle.with(|session| {
                let previous = session.directory.clone();
                session.record(json!({
                    "type": "location-switched",
                    "location": { "directory": directory.to_string_lossy() },
                    "projectID": crate::session::project_id(&directory),
                    "previous": { "location": { "directory": previous.to_string_lossy() } },
                }));
                session.directory = directory.clone();
                session.project = crate::session::project_id(&directory);
            });
            handle.save();
            app.plugins.emit(json!({
                "id": crate::util::id("evt"),
                "type": "session.moved",
                "properties": {
                    "sessionID": id,
                    "projectID": crate::session::project_id(&directory),
                    "location": { "directory": directory.to_string_lossy() }
                }
            }));
            Ok(Value::Null)
        }
        "session.abort" => {
            let id = session_id().ok_or_else(|| anyhow!("session.abort requires sessionID"))?;
            let handle = app.session(&id)?;
            for inbox_id in handle.abort() {
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.cancelled",
                    "properties": { "sessionID": id.clone(), "inboxID": inbox_id }
                }));
            }
            Ok(json!(true))
        }
        "session.interrupt" => {
            let id = session_id().ok_or_else(|| anyhow!("session.interrupt requires sessionID"))?;
            let handle = app.session(&id)?;
            let resume = first
                .get("resume")
                .or_else(|| first.pointer("/query/resume"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let interrupted = handle.is_busy();
            if let Some(cancel) = handle.cancel.lock().unwrap().as_ref() {
                cancel.cancel();
            }
            if !resume {
                for inbox_id in handle.abort() {
                    app.plugins.emit(json!({
                        "id": crate::util::id("evt"),
                        "type": "session.inbox.cancelled",
                        "properties": { "sessionID": id.clone(), "inboxID": inbox_id }
                    }));
                }
            }
            Ok(json!({ "interrupted": interrupted }))
        }
        "session.prompt" => {
            let id = session_id().ok_or_else(|| {
                anyhow!(
                    "session.prompt requires sessionID (got {})",
                    first
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                        .unwrap_or_else(|| first.to_string())
                )
            })?;
            app.session(&id)?;
            let mut texts: Vec<String> = Vec::new();
            let mut files: Vec<crate::session::FilePart> = Vec::new();
            if let Some(parts) = body.get("parts").and_then(Value::as_array) {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                texts.push(text.to_string());
                            }
                        }
                        Some("file") => {
                            let url = part.get("url").and_then(Value::as_str).unwrap_or_default();
                            if let Some((meta, data)) = url.split_once(";base64,") {
                                let mime = meta.trim_start_matches("data:").to_string();
                                files.push(crate::session::FilePart {
                                    mime,
                                    data: data.to_string(),
                                    name: part.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(attachments) = body
                .get("files")
                .or_else(|| body.pointer("/prompt/files"))
                .and_then(Value::as_array)
            {
                for attachment in attachments {
                    let name = attachment.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    if let Some(data) = attachment.get("data").and_then(Value::as_str) {
                        let mime = attachment
                            .get("mime")
                            .and_then(Value::as_str)
                            .unwrap_or("application/octet-stream");
                        files.push(crate::session::FilePart { mime: mime.to_string(), data: data.to_string(), name });
                        continue;
                    }
                    let uri = attachment.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if let Some((meta, data)) = uri.split_once(";base64,") {
                        files.push(crate::session::FilePart {
                            mime: meta.trim_start_matches("data:").to_string(),
                            data: data.to_string(),
                            name,
                        });
                    }
                }
            }
            if texts.is_empty() {
                if let Some(text) = body
                    .get("prompt")
                    .and_then(|v| v.get("text").and_then(Value::as_str).or_else(|| v.as_str()))
                    .or_else(|| body.get("text").and_then(Value::as_str))
                {
                    texts.push(text.to_string());
                }
            }
            let available_skills = app.skills();
            let extra = prompt_attachments(&available_skills, &body);
            let mut text = texts.join("\n");
            if !extra.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&extra);
            }
            let mut message = crate::session::Message::user_text(text);
            if let Some(id) = body.get("id").or_else(|| first.get("id")).and_then(Value::as_str) {
                message.id = id.to_string();
            }
            if let Some(metadata) = body.get("metadata") {
                message.metadata = Some(metadata.clone());
            }
            message.agents = body.get("agents").cloned();
            message.skills = body.get("skills").cloned();
            for file in files {
                message.parts.push(crate::session::Part::File(file));
            }
            if let Some(agent) = arg("agent") {
                message.agent = Some(agent);
            }
            let message_id = message.id.clone();
            let files = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    crate::session::Part::File(file) => Some(json!({
                        "data": file.data,
                        "mime": file.mime,
                        "source": { "type": "inline" },
                        "name": file.name,
                    })),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let payload_text = message.text();
            let mut opts = crate::app::PromptOptions::default();
            opts.agent = first.get("agent").and_then(Value::as_str).map(String::from);
            if let Some(model) = first.get("model") {
                let provider_id = model.get("providerID").and_then(Value::as_str);
                let model_id = model.get("id").or_else(|| model.get("modelID")).and_then(Value::as_str);
                if let (Some(provider_id), Some(model_id)) = (provider_id, model_id) {
                    opts.model = Some(format!("{provider_id}/{model_id}"));
                }
                opts.variant = model.get("variant").and_then(Value::as_str).map(String::from);
            }
            opts.delivery = body.get("delivery").and_then(Value::as_str).map(String::from);
            let mut payload = json!({ "text": payload_text, "files": files });
            if let Some(metadata) = body.get("metadata") {
                payload["metadata"] = metadata.clone();
            }
            if let Some(agents) = body.get("agents") {
                payload["agents"] = agents.clone();
            }
            if let Some(skills) = body.get("skills") {
                payload["skills"] = skills.clone();
            }
            app.spawn_prompt(&id, message, opts)?;
            Ok(json!({
                "id": message_id,
                "sessionID": id,
                "time": { "created": crate::util::now_ms() },
                "type": "user",
                "payload": payload,
                "delivery": body.get("delivery").and_then(Value::as_str).unwrap_or("steer"),
            }))
        }
        "session.synthetic" => {
            let id = session_id().ok_or_else(|| anyhow!("session.synthetic requires sessionID"))?;
            app.session(&id)?;
            let text = body.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            let message_id = body
                .get("id")
                .or_else(|| first.get("id"))
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| crate::util::id("msg"));
            let session_id = id.clone();
            let payload_text = text.clone();
            let notification_id = message_id.clone();
            let metadata = body.get("metadata").cloned();
            let app = app.clone();
            tokio::spawn(async move {
                crate::engine::notify_with_id(app, session_id, payload_text, Some(notification_id), metadata).await
            });
            let mut payload = json!({ "text": text });
            if let Some(description) = body.get("description") {
                payload["description"] = description.clone();
            }
            if let Some(metadata) = body.get("metadata") {
                payload["metadata"] = metadata.clone();
            }
            Ok(json!({
                "id": message_id,
                "sessionID": id,
                "time": { "created": crate::util::now_ms() },
                "type": "synthetic",
                "payload": payload,
                "delivery": body.get("delivery").and_then(Value::as_str).unwrap_or("steer"),
            }))
        }
        "session.command" => {
            let id = session_id().ok_or_else(|| anyhow!("session.command requires sessionID"))?;
            app.session(&id)?;
            let name = body
                .get("command")
                .or_else(|| body.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| arg("command"))
                .or_else(|| arg("name"))
                .unwrap_or_default();
            let arguments = body
                .get("arguments")
                .or_else(|| body.get("text"))
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| arg("arguments"))
                .or_else(|| arg("text"))
                .unwrap_or_default();
            if app.plugin_commands().contains(&name) {
                let prompt = json!({
                    "text": arguments,
                    "files": body.get("files").cloned().unwrap_or_else(|| json!([])),
                    "agents": body.get("agents").cloned().unwrap_or_else(|| json!([])),
                    "skills": body.get("skills").cloned().unwrap_or_else(|| json!([])),
                });
                let delivery = body.get("delivery").and_then(Value::as_str).unwrap_or("steer");
                let _ = app.plugins.execute_command(&name, &id, prompt, delivery).await?;
                Ok(json!(true))
            } else if let Some(cmd) = app.commands().get(&name).cloned() {
                let mut text = crate::command::render(&cmd, &arguments, &app.directory, &app.worktree);
                let available_skills = app.skills();
                let extra = prompt_attachments(&available_skills, &body);
                if !extra.is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&extra);
                }
                let mut opts = crate::app::PromptOptions::default();
                opts.agent = cmd.agent.clone();
                opts.model = cmd.model.clone();
                opts.delivery = body.get("delivery").and_then(Value::as_str).map(String::from);
                let mut message = crate::session::Message::user_text(text);
                for file in prompt_files(&body) {
                    message.parts.push(crate::session::Part::File(file));
                }
                app.spawn_prompt(&id, message, opts)?;
                Ok(json!(true))
            } else {
                Err(anyhow!("command not found: {name}"))
            }
        }
        "session.compact" | "session.summarize" => {
            let id = session_id().ok_or_else(|| anyhow!("session.compact requires sessionID"))?;
            let handle = app.session(&id)?;
            let delivery = body.get("delivery").and_then(Value::as_str).unwrap_or("steer");
            let message_id = body
                .get("id")
                .or_else(|| first.get("id"))
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| crate::util::id("msg"));
            if handle.is_busy() {
                *handle.pending_compaction.lock().unwrap() = Some(message_id.clone());
                app.plugins.emit(json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.enqueued",
                    "properties": {
                        "sessionID": id.clone(),
                        "inboxID": message_id.clone(),
                        "item": { "type": "compaction", "payload": {}, "delivery": delivery }
                    }
                }));
            } else {
                let app = app.clone();
                let session = id.clone();
                tokio::spawn(async move {
                    let _ = crate::engine::compact_session(app, &session).await;
                });
            }
            Ok(json!({
                "id": message_id,
                "sessionID": id,
                "time": { "created": crate::util::now_ms() },
                "type": "compaction",
                "payload": {},
                "delivery": delivery,
            }))
        }
        "permission.list" => {
            let filter = first.get("sessionID").and_then(Value::as_str);
            Ok(json!(app
                .pending_permissions()
                .iter()
                .filter(|request| filter.is_none_or(|session| request.session_id == session))
                .map(permission_json)
                .collect::<Vec<_>>()))
        }
        "permission.get" => {
            let id = arg("requestID").or_else(|| arg("id")).unwrap_or_default();
            let session = arg("sessionID");
            Ok(app
                .pending_permissions()
                .iter()
                .find(|request| {
                    request.id == id
                        && session.as_deref().is_none_or(|session| request.session_id == session)
                })
                .map(permission_json)
                .unwrap_or(Value::Null))
        }
        "permission.reply" => {
            let id = arg("requestID")
                .or_else(|| arg("id"))
                .or_else(|| first.pointer("/path/id").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            let decision = body
                .get("reply")
                .and_then(Value::as_str)
                .or_else(|| body.get("decision").and_then(Value::as_str))
                .or_else(|| first.get("reply").and_then(Value::as_str))
                .or_else(|| first.get("decision").and_then(Value::as_str));
            let reply = match decision {
                Some("always") | Some("allow-always") => crate::permission::Reply::Always,
                Some("reject") | Some("deny") => crate::permission::Reply::Reject,
                _ => crate::permission::Reply::Once,
            };
            app.reply_permission(&id, reply);
            Ok(Value::Null)
        }
        "agent.list" | "app.agents" => Ok(json!(app.agents().values().map(agent_json).collect::<Vec<_>>())),
        "command.list" | "app.commands" => Ok(json!(app
            .commands()
            .values()
            .map(|c| json!({ "name": c.name, "description": c.description }))
            .collect::<Vec<_>>())),
        "skill.list" | "app.skills" => Ok(json!(app
            .skills()
            .iter()
            .map(|skill| json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "path": skill.path.to_string_lossy(),
                "content": skill.content,
                "autoinvoke": true,
            }))
            .collect::<Vec<_>>())),
        "provider.list" | "config.providers" => Ok(json!(app
            .providers()
            .values()
            .map(|provider| json!({
                "id": provider.id,
                "name": provider.name,
                "package": provider.npm,
                "activation": "auto",
                "settings": {},
                "headers": provider.options.headers,
                "body": {},
            }))
            .collect::<Vec<_>>())),
        "model.list" => Ok(json!(app
            .providers()
            .values()
            .flat_map(|provider| provider.models.values().map(model_json).collect::<Vec<_>>())
            .collect::<Vec<_>>())),
        "mcp.list" => Ok(json!(app
            .mcp
            .snapshot()
            .into_iter()
            .map(|(name, status)| json!({
                "name": name,
                "status": match status {
                    Some(crate::mcp::Status::Connected { tools }) => json!({ "status": "connected", "tools": tools }),
                    Some(crate::mcp::Status::Disabled) => json!({ "status": "disabled" }),
                    Some(crate::mcp::Status::Failed(error)) => json!({ "status": "failed", "error": error }),
                    None => json!({ "status": "pending" }),
                },
            }))
            .collect::<Vec<_>>())),
        "session.children" => {
            let id = session_id().ok_or_else(|| anyhow!("session.children requires sessionID"))?;
            app.session(&id)?;
            Ok(json!(crate::session::list(&app.project)
                .into_iter()
                .filter(|s| s.parent_id.as_deref() == Some(id.as_str()))
                .map(|s| session_json_summary(&s))
                .collect::<Vec<_>>()))
        }
        "session.todo" => {
            let id = session_id().ok_or_else(|| anyhow!("session.todo requires sessionID"))?;
            let handle = app.session(&id)?;
            Ok(handle.with(|s| json!(s.todos)))
        }
        "tool.list" | "tool.ids" => {
            let tools = app.plugins.tool_list().await;
            Ok(json!(tools
                .iter()
                .map(|t| json!({ "id": t.id, "name": t.name, "description": t.description }))
                .collect::<Vec<_>>()))
        }
        "plugin.list" => Ok(json!(app.plugins.list())),
        "generate.text" => {
            let prompt = first.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
            let model = first.get("model").cloned();
            plugin_generate(app, "plugin", Some(app.default_agent()), None, prompt, model, false).await
        }
        "generate.image" | "experimental.generate.image" => {
            let reference = generate_ref(&first).ok_or_else(|| anyhow!("generate.image requires a model"))?;
            let resolved = crate::models::resolve_media(
                &app.providers(),
                &reference,
                crate::models::ModelKind::Image,
            )?;
            crate::provider::media::generate_image(&resolved.provider, &resolved.model, &first).await
        }
        "generate.decisions" | "experimental.generate.decisions" => {
            let reference = generate_ref(&first).ok_or_else(|| anyhow!("generate.decisions requires a model"))?;
            let resolved = crate::models::resolve_media(
                &app.providers(),
                &reference,
                crate::models::ModelKind::Decision,
            )?;
            crate::provider::media::generate_decisions(&resolved.provider, &resolved.model, &first).await
        }
        // The Rust client keeps provider keys in `auth.json` rather than a
        // multi-credential integration store. Activation is therefore already
        // implied by the provider credential selected through `auth`.
        "credential.activate" => Ok(Value::Null),
        "experimental.terminal.read" => {
            let id = session_id().ok_or_else(|| anyhow!("experimental.terminal.read requires sessionID"))?;
            Ok(pty::manager().read(&id).unwrap_or(Value::Null))
        }
        path if path.starts_with("vcs.") || path.starts_with("worktree.") => {
            crate::vcs::dispatch(path, &first)
        }
        _ => Err(anyhow!("plugin client: unsupported method {path}")),
    }
}

pub(crate) fn agent_json(agent: &crate::agent::Agent) -> Value {
    json!({
        "id": agent.id,
        "name": agent.name,
        "description": agent.description,
        "mode": match agent.mode {
            crate::agent::Mode::Primary => "primary",
            crate::agent::Mode::Subagent => "subagent",
            crate::agent::Mode::All => "all",
        },
        "hidden": agent.hidden,
        "system": agent.system,
        "model": agent.model,
        "steps": agent.steps,
        "color": agent.color,
        "permissions": agent.permissions.iter().map(|r| json!({
            "action": r.action,
            "resource": r.resource,
            "effect": match r.effect {
                crate::permission::Effect::Allow => "allow",
                crate::permission::Effect::Deny => "deny",
                crate::permission::Effect::Ask => "ask",
            }
        })).collect::<Vec<_>>(),
    })
}

fn permission_json(request: &crate::permission::Request) -> Value {
    json!({
        "id": request.id,
        "sessionID": request.session_id,
        "action": request.action,
        "resources": request.resources,
        "save": request.save,
        "metadata": request.metadata,
        "message": request.title,
    })
}

fn model_ref_json(reference: Option<&str>) -> Value {
    reference
        .and_then(crate::models::parse_ref)
        .map(|(provider_id, id, variant)| json!({ "providerID": provider_id, "id": id, "variant": variant }))
        .unwrap_or(Value::Null)
}

fn shell_message_json(shell: &crate::session::ShellMessage) -> Value {
    let mut value = json!({
        "id": shell.id,
        "time": { "created": shell.started },
        "type": "shell",
        "shellID": shell.id,
        "command": shell.command,
        "status": shell.status,
    });
    if let Some(completed) = shell.completed {
        value["time"]["completed"] = json!(completed);
    }
    if let Some(exit) = shell.exit {
        value["exit"] = json!(exit);
    }
    if let Some(output) = &shell.output {
        value["output"] = json!({
            "output": output,
            "cursor": output.len(),
            "size": output.len(),
            "truncated": false,
        });
    }
    value
}

fn session_history_json(
    messages: &[crate::session::Message],
    shells: &[crate::session::ShellMessage],
    records: &[Value],
) -> Value {
    let mut values = session_messages_json(messages).as_array().cloned().unwrap_or_default();
    values.extend(shells.iter().map(shell_message_json));
    values.extend(records.iter().cloned());
    values.sort_by_key(|value| {
        value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or_default()
    });
    Value::Array(values)
}

pub(crate) fn session_messages_json(messages: &[crate::session::Message]) -> Value {
    use crate::session::{Part, Role, ToolState};
    json!(messages
        .iter()
        .map(|message| match message.role {
            Role::User => {
                if let Some(summary) = message.parts.iter().find_map(|part| match part {
                    Part::Compaction { summary, .. } => Some(summary),
                    _ => None,
                }) {
                    let metadata = message.parts.iter().find_map(|part| match part {
                        Part::Compaction { metadata, .. } => metadata.clone(),
                        _ => None,
                    });
                    let mut value = json!({
                        "id": message.id,
                        "time": { "created": message.time },
                        "type": "compaction",
                        "status": "completed",
                        "reason": "auto",
                        "summary": summary,
                        "cost": message.cost,
                        "tokens": {
                            "input": message.usage.input,
                            "output": message.usage.output,
                            "reasoning": message.usage.reasoning,
                            "cache": { "read": message.usage.cache_read, "write": message.usage.cache_write },
                        },
                    });
                    if let Some(metadata) = metadata {
                        value["metadata"] = metadata;
                    }
                    return value;
                }
                let text_parts: Vec<(&str, bool)> = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text, synthetic } => Some((text.as_str(), *synthetic)),
                        _ => None,
                    })
                    .collect();
                let text = text_parts.iter().map(|(text, _)| *text).collect::<Vec<_>>().join("\n");
                if !text_parts.is_empty()
                    && text_parts.iter().all(|(_, synthetic)| *synthetic)
                    && !message.parts.iter().any(|part| matches!(part, Part::File(_)))
                {
                    return json!({
                        "id": message.id,
                        "time": { "created": message.time },
                        "type": "synthetic",
                        "text": text,
                    });
                }
                let files: Vec<Value> = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::File(file) => Some(json!({
                            "data": file.data,
                            "mime": file.mime,
                            "source": { "type": "inline" },
                            "name": file.name,
                        })),
                        _ => None,
                    })
                    .collect();
                let mut value = json!({
                    "id": message.id,
                    "time": { "created": message.time },
                    "type": "user",
                    "text": text,
                });
                if let Some(metadata) = &message.metadata {
                    value["metadata"] = metadata.clone();
                }
                if let Some(agents) = &message.agents {
                    value["agents"] = agents.clone();
                }
                if let Some(skills) = &message.skills {
                    value["skills"] = skills.clone();
                }
                if !files.is_empty() {
                    value["files"] = json!(files);
                }
                value
            }
            Role::Assistant => {
                let content: Vec<Value> = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
                        Part::Reasoning { text, .. } => Some(json!({ "type": "reasoning", "text": text })),
                        Part::Tool { id, name, input, state, .. } => Some(json!({
                            "type": "tool",
                            "id": id,
                            "name": name,
                            "input": input,
                            "state": match state {
                                ToolState::Pending => json!({
                                    "status": "streaming", "input": input.to_string(),
                                }),
                                ToolState::Running => json!({
                                    "status": "running", "input": input, "metadata": {},
                                }),
                                ToolState::Completed { output, metadata, attachments, .. } => {
                                    let mut items = vec![json!({ "type": "text", "text": output })];
                                    items.extend(attachments.iter().map(|file| json!({
                                        "type": "file",
                                        "uri": format!("data:{};base64,{}", file.mime, file.data),
                                        "mime": file.mime,
                                        "name": file.name,
                                    })));
                                    json!({
                                        "status": "completed", "input": input, "content": items, "metadata": metadata,
                                    })
                                }
                                ToolState::Error { error } => json!({
                                    "status": "error",
                                    "input": input,
                                    "error": { "type": "unknown", "message": error },
                                }),
                            },
                            "time": { "created": message.time },
                        })),
                        Part::Compaction { summary, .. } => Some(json!({ "type": "text", "text": summary })),
                        Part::File(_) => None,
                    })
                    .collect();
                let mut time = json!({ "created": message.time });
                if let Some(completed) = message.completed {
                    time["completed"] = json!(completed);
                }
                let mut value = json!({
                    "id": message.id,
                    "time": time,
                    "type": "assistant",
                    "agent": message.agent.clone().unwrap_or_default(),
                    "model": model_ref_json(message.model.as_deref()),
                    "content": content,
                    "cost": message.cost,
                    "tokens": {
                        "input": message.usage.input,
                        "output": message.usage.output,
                        "reasoning": message.usage.reasoning,
                        "cache": { "read": message.usage.cache_read, "write": message.usage.cache_write },
                    },
                });
                if let Some(metadata) = &message.metadata {
                    value["metadata"] = metadata.clone();
                }
                if let Some(finish) = &message.finish {
                    value["finish"] = json!(finish);
                }
                if let Some(error) = &message.error {
                    value["error"] = json!({ "type": "unknown", "message": error });
                }
                value
            }
        })
        .collect::<Vec<_>>())
}

pub(crate) fn session_json_summary(session: &crate::session::Summary) -> Value {
    let mut value = json!({
        "id": session.id,
        "projectID": crate::session::project_id(&session.directory),
        "title": session.title,
        "cost": session.cost,
        "tokens": {
            "input": session.tokens.input,
            "output": session.tokens.output,
            "reasoning": session.tokens.reasoning,
            "cache": { "read": session.tokens.cache_read, "write": session.tokens.cache_write },
        },
        "time": {
            "created": session.time_created,
            "updated": session.time_updated,
        },
        "location": { "directory": session.directory.to_string_lossy() },
    });
    if let Some(parent) = &session.parent_id {
        value["parentID"] = json!(parent);
    }
    if let Some(metadata) = &session.metadata {
        value["metadata"] = metadata.clone();
    }
    if let Some(permissions) = &session.permissions {
        value["permissions"] = permissions.clone();
    }
    if let Some(revert) = &session.revert {
        value["revert"] = revert.clone();
    }
    if let Some(viewed) = session.viewed_at {
        value["time"]["viewed"] = json!(viewed);
    }
    if let Some(idle) = session.idle_ms {
        value["time"]["idle"] = json!(idle);
    }
    if let Some(busy) = session.busy_at {
        value["time"]["busy"] = json!(busy);
    }
    if let Some(outcome) = &session.last_outcome {
        value["outcome"] = json!(outcome);
    }
    if let Some(fork) = &session.fork {
        value["fork"] = fork.clone();
    }
    if session.archived {
        value["time"]["archived"] = json!(session.time_updated);
    }
    value
}

pub(crate) fn session_json(session: &crate::session::Session) -> Value {
    let (usage, cost) = session.usage_total();
    let mut value = json!({
        "id": session.id,
        "projectID": session.project,
        "title": session.title,
        "cost": cost,
        "tokens": {
            "input": usage.input,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "cache": { "read": usage.cache_read, "write": usage.cache_write },
        },
        "time": {
            "created": session.time_created,
            "updated": session.time_updated,
        },
        "location": { "directory": session.directory.to_string_lossy() },
    });
    if let Some(parent) = &session.parent_id {
        value["parentID"] = json!(parent);
    }
    if let Some(agent) = &session.agent {
        value["agent"] = json!(agent);
    }
    if let Some((provider_id, id, variant)) = session.model.as_deref().and_then(crate::models::parse_ref) {
        value["model"] = json!({ "providerID": provider_id, "id": id, "variant": variant });
    }
    if let Some(metadata) = &session.metadata {
        value["metadata"] = metadata.clone();
    }
    if let Some(permissions) = &session.permissions {
        value["permissions"] = permissions.clone();
    }
    if let Some(revert) = &session.revert {
        value["revert"] = revert.clone();
    }
    if let Some(viewed) = session.viewed_at {
        value["time"]["viewed"] = json!(viewed);
    }
    if let Some(idle) = session.idle_ms {
        value["time"]["idle"] = json!(idle);
    }
    if let Some(busy) = session.busy_at {
        value["time"]["busy"] = json!(busy);
    }
    if let Some(outcome) = &session.last_outcome {
        value["outcome"] = json!(outcome);
    }
    if let Some(fork) = &session.fork {
        value["fork"] = fork.clone();
    }
    if session.archived {
        value["time"]["archived"] = json!(session.time_updated);
    }
    value
}

// ---------------------------------------------------------------- state conversion

pub struct Applied {
    pub agents: BTreeMap<String, crate::agent::Agent>,
    pub commands: BTreeMap<String, crate::command::Command>,
    pub plugin_commands: BTreeSet<String>,
    pub skills: Vec<crate::tools::skill::Skill>,
    pub mcp: BTreeMap<String, crate::config::McpConfig>,
    pub providers: BTreeMap<String, crate::models::Provider>,
    pub default_agent: Option<String>,
    pub default_model: Option<String>,
}

pub fn snapshot(
    config: &crate::config::Config,
    providers: &BTreeMap<String, crate::models::Provider>,
    agents: &BTreeMap<String, crate::agent::Agent>,
    skills: &[crate::tools::skill::Skill],
    commands: &BTreeMap<String, crate::command::Command>,
    directory: &Path,
    worktree: &Path,
    project: &str,
) -> Value {
    let agents: Vec<Value> = agents
        .values()
        .map(|agent| {
            let mut value = agent_json(agent);
            value["model"] = match &agent.model {
                Some(model) => crate::models::parse_ref(model)
                    .map(|(provider_id, id, variant)| json!({ "providerID": provider_id, "id": id, "variant": variant }))
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            value["request"] = json!({
                "settings": {
                    "temperature": agent.temperature,
                    "topP": agent.top_p,
                },
                "headers": {},
                "body": {},
            });
            value
        })
        .collect();
    let providers: Vec<Value> = providers
        .values()
        .map(|provider| {
            json!({
                "info": {
                    "id": provider.id,
                    "name": provider.name,
                    "activation": "auto",
                    "package": provider.npm,
                    "settings": provider_settings(provider),
                    "headers": provider.options.headers,
                    "body": {},
                },
                "models": provider.models.values().map(model_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    let skills: Vec<Value> = skills
        .iter()
        .map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "path": skill.path.to_string_lossy(),
                "content": skill.content,
                "autoinvoke": true,
            })
        })
        .collect();
    let commands: Vec<Value> = commands
        .values()
        .map(|command| json!({ "name": command.name, "description": command.description }))
        .collect();
    let tools: Vec<Value> = crate::tools::descriptors(config)
        .iter()
        .map(|(name, description, schema)| {
            json!({
                "id": name,
                "name": name,
                "description": description,
                "input": schema,
                "options": {},
                "source": "builtin",
            })
        })
        .collect();
    json!({
        "config": serde_json::to_value(config).unwrap_or_else(|_| json!({})),
        "agents": agents,
        "providers": providers,
        "skills": skills,
        "commands": commands,
        "tools": tools,
        "websearch": crate::tools::websearch::providers()
            .into_iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect::<Vec<_>>(),
        "mcp": config.mcp.iter().map(|(name, value)| json!([name, mcp_json(value)])).collect::<Vec<_>>(),
        "defaultAgent": config.default_agent,
        "defaultModel": config.model.as_ref().and_then(|m| crate::models::parse_ref(m)).map(|(provider_id, id, variant)| json!({
            "providerID": provider_id,
            "id": id,
            "variant": variant,
        })),
        "directory": directory.to_string_lossy(),
        "worktree": worktree.to_string_lossy(),
        "project": project,
    })
}

pub(crate) fn provider_settings(provider: &crate::models::Provider) -> Value {
    let mut settings = serde_json::Map::new();
    if let Some(base_url) = &provider.options.base_url {
        settings.insert("baseURL".into(), json!(base_url));
    }
    for (key, value) in &provider.options.extra {
        settings.insert(key.clone(), value.clone());
    }
    Value::Object(settings)
}

pub(crate) fn model_json(model: &crate::models::Model) -> Value {
    json!({
        "id": model.id,
        "modelID": model.id,
        "providerID": model.provider_id,
        "type": match model.kind {
            crate::models::ModelKind::Chat => "chat",
            crate::models::ModelKind::Image => "image",
            crate::models::ModelKind::Decision => "decision",
        },
        "name": model.name,
        "family": model.family,
        "capabilities": {
            "tools": model.tool_call,
            "input": model.input_modalities,
            "output": ["text"],
            "temperature": model.temperature,
        },
        "variants": [],
        "time": { "released": 0 },
        "cost": [{
            "input": model.cost_input,
            "output": model.cost_output,
            "cache": { "read": model.cost_cache_read, "write": model.cost_cache_write },
        }],
        "status": model.status.clone().unwrap_or_else(|| "active".into()),
        "enabled": true,
        "limit": { "context": model.context, "output": model.output },
        "settings": {},
        "headers": model.headers,
        "body": model.options,
    })
}

fn mcp_json(config: &crate::config::McpConfig) -> Value {
    let mut value = if let Some(url) = &config.url {
        json!({
            "type": "remote",
            "url": url,
            "headers": config.headers,
            "disabled": !config.enabled.unwrap_or(true),
        })
    } else {
        json!({
            "type": "local",
            "command": config.command,
            "environment": config.environment,
            "disabled": !config.enabled.unwrap_or(true),
        })
    };
    if let Some(cwd) = &config.cwd {
        value["cwd"] = json!(cwd);
    }
    if let Some(codemode) = config.codemode {
        value["codemode"] = json!(codemode);
    }
    if let Some(oauth) = &config.oauth {
        value["oauth"] = oauth.clone();
    }
    if let Some(protocol) = &config.protocol {
        value["protocol"] = json!(protocol);
    }
    if let Some(pool) = config.pool {
        value["pool"] = json!(pool);
    }
    if let Some(timeout) = &config.timeout {
        value["timeout"] = timeout.clone();
    }
    value
}

fn apply_permissions(value: &Value) -> Vec<crate::permission::Rule> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let action = item.get("action").and_then(Value::as_str)?;
            let resource = item.get("resource").and_then(Value::as_str)?;
            let effect = item.get("effect").and_then(Value::as_str).and_then(crate::permission::Effect::parse)?;
            Some(crate::permission::rule(action, resource, effect))
        })
        .collect()
}

pub fn apply_state(
    value: &Value,
    config: &crate::config::Config,
    base_providers: &BTreeMap<String, crate::models::Provider>,
    base_agents: &BTreeMap<String, crate::agent::Agent>,
    base_commands: &BTreeMap<String, crate::command::Command>,
) -> Applied {
    let model_ref = |value: &Value| -> Option<String> {
        let provider_id = value.get("providerID").and_then(Value::as_str)?;
        let id = value.get("id").or_else(|| value.get("modelID")).and_then(Value::as_str)?;
        let variant = value.get("variant").and_then(Value::as_str);
        Some(match variant {
            Some(variant) if !variant.is_empty() => format!("{provider_id}/{id}#{variant}"),
            _ => format!("{provider_id}/{id}"),
        })
    };

    let mut agents = BTreeMap::new();
    for item in value.get("agents").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
        let mode = match item.get("mode").and_then(Value::as_str) {
            Some("subagent") => crate::agent::Mode::Subagent,
            Some("all") => crate::agent::Mode::All,
            _ => crate::agent::Mode::Primary,
        };
        let settings = item.pointer("/request/settings").cloned().unwrap_or(Value::Null);
        let model = item.get("model").and_then(&model_ref);
        let mut agent = base_agents.get(id).cloned().unwrap_or_else(|| {
            let mut agent = crate::agent::Agent {
                id: id.to_string(),
                name: id.to_string(),
                description: None,
                mode: crate::agent::Mode::Primary,
                hidden: false,
                builtin: false,
                system: None,
                model: None,
                variant: None,
                temperature: None,
                top_p: None,
                steps: None,
                permissions: Vec::new(),
                tools: BTreeMap::new(),
                color: None,
            };
            agent.name = id.to_string();
            agent
        });
        agent.name = item.get("name").and_then(Value::as_str).unwrap_or(id).to_string();
        agent.description = item.get("description").and_then(Value::as_str).map(String::from);
        agent.mode = mode;
        agent.hidden = item.get("hidden").and_then(Value::as_bool).unwrap_or(false);
        agent.system = item.get("system").and_then(Value::as_str).map(String::from);
        agent.model = model;
        agent.variant = item.pointer("/model/variant").and_then(Value::as_str).map(String::from);
        agent.temperature = settings.get("temperature").and_then(Value::as_f64);
        agent.top_p = settings.get("topP").and_then(Value::as_f64);
        agent.steps = item.get("steps").and_then(Value::as_u64).map(|v| v as usize);
        agent.permissions = apply_permissions(item.get("permissions").unwrap_or(&Value::Null));
        agent.color = item.get("color").and_then(Value::as_str).map(String::from);
        agents.insert(id.to_string(), agent);
    }
    if value.get("agents").and_then(Value::as_array).is_none() {
        agents = crate::agent::load(config, &[]);
    }

    let mut commands = base_commands.clone();
    let mut plugin_commands = BTreeSet::new();
    for item in value.get("commands").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = item.get("name").and_then(Value::as_str) else { continue };
        let plugin = item.get("plugin").and_then(Value::as_bool).unwrap_or(false);
        if plugin {
            plugin_commands.insert(name.to_string());
            commands.insert(
                name.to_string(),
                crate::command::Command {
                    name: name.to_string(),
                    description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                    template: String::new(),
                    agent: None,
                    model: None,
                    mcp_prompt: None,
                },
            );
        } else if let Some(command) = commands.get_mut(name) {
            if let Some(description) = item.get("description").and_then(Value::as_str) {
                command.description = description.to_string();
            }
        }
    }

    let mut skills = Vec::new();
    for item in value.get("skills").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
        skills.push(crate::tools::skill::Skill {
            id: id.to_string(),
            name: item.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
            description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
            path: PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or_default()),
            content: item.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
        });
    }

    let mut mcp = BTreeMap::new();
    for entry in value.get("mcp").and_then(Value::as_array).into_iter().flatten() {
        let Some(pair) = entry.as_array() else { continue };
        let (Some(name), Some(config_value)) = (pair.first().and_then(Value::as_str), pair.get(1)) else { continue };
        let kind = config_value.get("type").and_then(Value::as_str);
        let mut server = crate::config::McpConfig {
            kind: kind.map(String::from),
            command: config_value
                .get("command")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            environment: string_map(config_value.get("environment")),
            cwd: config_value.get("cwd").and_then(Value::as_str).map(String::from),
            codemode: config_value.get("codemode").and_then(Value::as_bool),
            url: config_value.get("url").and_then(Value::as_str).map(String::from),
            headers: string_map(config_value.get("headers")),
            oauth: config_value.get("oauth").cloned(),
            protocol: config_value.get("protocol").and_then(Value::as_str).map(String::from),
            pool: config_value.get("pool").and_then(Value::as_bool),
            enabled: config_value.get("disabled").and_then(Value::as_bool).map(|disabled| !disabled),
            timeout: config_value.get("timeout").cloned(),
        };
        if server.enabled.is_none() {
            server.enabled = Some(true);
        }
        if let Some(base) = config.mcp.get(name) {
            if server.timeout.is_none() {
                server.timeout = base.timeout.clone();
            }
        }
        mcp.insert(name.to_string(), server);
    }
    if value.get("mcp").and_then(Value::as_array).is_none() {
        mcp = config.mcp.clone();
    }

    let mut providers = BTreeMap::new();
    for item in value.get("providers").and_then(Value::as_array).into_iter().flatten() {
        let Some(info) = item.get("info") else { continue };
        let Some(id) = info.get("id").and_then(Value::as_str) else { continue };
        let base = base_providers.get(id);
        let npm = info
            .get("package")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| base.map(|p| p.npm.as_str()).unwrap_or("@ai-sdk/openai-compatible"))
            .to_string();
        let mut models = BTreeMap::new();
        for model in item.get("models").and_then(Value::as_array).into_iter().flatten() {
            if let Some(model) = model_from_value(id, model, base.and_then(|p| p.models.get(model.get("id").and_then(Value::as_str).unwrap_or("")))) {
                models.insert(model.id.clone(), model);
            }
        }
        let mut options = base.map(|p| p.options.clone()).unwrap_or_default();
        if let Some(headers) = info.get("headers").and_then(Value::as_object) {
            for (key, value) in headers {
                if let Some(value) = value.as_str() {
                    options.headers.insert(key.clone(), value.to_string());
                }
            }
        }
        for source in [info.get("settings"), info.get("body")].into_iter().flatten() {
            let Some(values) = source.as_object() else { continue };
            for (key, value) in values {
                if key == "baseURL" {
                    if let Some(url) = value.as_str() {
                        options.base_url = Some(url.to_string());
                    }
                    continue;
                }
                options.extra.insert(key.clone(), value.clone());
            }
        }
        providers.insert(
            id.to_string(),
            crate::models::Provider {
                id: id.to_string(),
                name: info.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
                npm,
                api: base.and_then(|p| p.api.clone()),
                env: base.map(|p| p.env.clone()).unwrap_or_default(),
                key: base.and_then(|p| p.key.clone())
                    .or_else(|| crate::auth::get(id).map(|credential| credential.secret().to_string()))
                    .or_else(|| {
                        let env = format!("{}_API_KEY", id.to_uppercase().replace('-', "_"));
                        std::env::var(env).ok().filter(|key| !key.is_empty())
                    }),
                options,
                models,
                media_models: std::collections::BTreeMap::new(),
            },
        );
    }
    if value.get("providers").and_then(Value::as_array).is_none() {
        providers = base_providers.clone();
    }

    Applied {
        agents,
        commands,
        plugin_commands,
        skills,
        mcp,
        providers,
        default_agent: value.get("defaultAgent").and_then(Value::as_str).map(String::from),
        default_model: value.get("defaultModel").and_then(&model_ref),
    }
}

/// Convert messages to the `{ info, parts }` shape used by V1's
/// `experimental.chat.messages.transform` and V2's session hooks.
pub fn messages_for_hook(session_id: &str, messages: &[crate::session::Message]) -> Value {
    use crate::session::{Part, ToolState};
    let parts = |message: &crate::session::Message| -> Vec<Value> {
        message
            .parts
            .iter()
            .map(|part| match part {
                Part::Text { text, synthetic } => json!({
                    "type": "text", "text": text, "synthetic": synthetic,
                }),
                Part::Reasoning { text, .. } => json!({ "type": "reasoning", "text": text }),
                Part::Tool { id, name, input, state, .. } => json!({
                    "type": "tool",
                    "tool": name,
                    "callID": id,
                    "state": match state {
                        ToolState::Pending => json!({ "status": "pending", "input": input }),
                        ToolState::Running => json!({ "status": "running", "input": input }),
                        ToolState::Completed { output, title, metadata, attachments } => json!({
                            "status": "completed", "input": input, "output": output, "title": title, "metadata": metadata,
                            "attachments": attachments.iter().map(|file| json!({
                                "type": "file", "mime": file.mime, "filename": file.name,
                                "url": format!("data:{};base64,{}", file.mime, file.data),
                            })).collect::<Vec<_>>(),
                        }),
                        ToolState::Error { error } => json!({ "status": "error", "input": input, "error": error }),
                    }
                }),
                Part::File(file) => json!({
                    "type": "file", "mime": file.mime, "filename": file.name,
                    "url": format!("data:{};base64,{}", file.mime, file.data),
                }),
                Part::Compaction { summary, metadata } => json!({
                    "type": "compaction", "summary": summary, "metadata": metadata,
                }),
            })
            .collect()
    };
    json!(messages
        .iter()
        .map(|message| {
            let role = match message.role {
                crate::session::Role::User => "user",
                crate::session::Role::Assistant => "assistant",
            };
            json!({
                "info": {
                    "id": message.id,
                    "role": role,
                    "sessionID": session_id,
                    "time": { "created": message.time },
                    "agent": message.agent,
                    "model": message.model.as_deref().and_then(crate::models::parse_ref).map(|(provider_id, id, variant)| json!({
                        "providerID": provider_id, "modelID": id, "variant": variant
                    })),
                    "finish": message.finish,
                    "error": message.error,
                },
                "parts": parts(message),
            })
        })
        .collect::<Vec<_>>())
}

/// Apply a plugin's edit of the `{ info, parts }` list back onto messages.
/// Unknown/removed entries keep the original data.
pub fn apply_messages_hook(messages: &mut Vec<crate::session::Message>, value: &Value) {
    use crate::session::{Part, ToolState};
    let Some(items) = value.as_array() else { return };
    if items.is_empty() && !messages.is_empty() {
        return;
    }
    let mut result = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let info = item.get("info");
        let id = info.and_then(|info| info.get("id")).and_then(Value::as_str);
        let existing = match id {
            Some(id) => messages.iter().position(|m| m.id == id).and_then(|index| messages.get(index)).cloned(),
            None => messages.get(index).cloned(),
        };
        let mut message = match existing {
            Some(message) => message,
            None => {
                // A context transform may add synthetic head messages (for
                // example Magic Context's m[0]/m[1]) that have no host row.
                let role = match info.and_then(|info| info.get("role")).and_then(Value::as_str) {
                    Some("assistant") => crate::session::Role::Assistant,
                    _ => crate::session::Role::User,
                };
                let mut message = crate::session::Message::new(role);
                if let Some(id) = id {
                    message.id = id.to_string();
                }
                message
            }
        };
        if let Some(info) = info {
            if let Some(role) = info.get("role").and_then(Value::as_str) {
                message.role = match role {
                    "assistant" => crate::session::Role::Assistant,
                    _ => crate::session::Role::User,
                };
            }
            if let Some(agent) = info.get("agent").and_then(Value::as_str) {
                message.agent = Some(agent.to_string());
            }
            if let Some(finish) = info.get("finish").and_then(Value::as_str) {
                message.finish = Some(finish.to_string());
            }
            if let Some(error) = info.get("error").and_then(Value::as_str) {
                message.error = Some(error.to_string());
            }
        }
        let Some(parts) = item.get("parts").and_then(Value::as_array) else { continue };
        let mut next = Vec::with_capacity(parts.len());
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => next.push(Part::Text {
                    text: part.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                    synthetic: part.get("synthetic").and_then(Value::as_bool).unwrap_or(false),
                }),
                Some("reasoning") => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                    let existing = message.parts.iter().find_map(|p| match p {
                        Part::Reasoning { text: current, signature, redacted, item_id, started, ended } if *current == text => {
                            Some((signature.clone(), redacted.clone(), item_id.clone(), *started, *ended))
                        }
                        _ => None,
                    });
                    let (signature, redacted, item_id, started, ended) = existing.unwrap_or((None, None, None, None, None));
                    next.push(Part::Reasoning { text, signature, redacted, item_id, started, ended });
                }
                Some("tool") => {
                    let call = part.get("callID").and_then(Value::as_str).unwrap_or_default();
                    let existing = message.parts.iter().find_map(|p| match p {
                        Part::Tool { id, name, input, state, provider_meta } if id == call => {
                            let attachments = match state {
                                ToolState::Completed { attachments, .. } => attachments.clone(),
                                _ => Vec::new(),
                            };
                            Some((name.clone(), input.clone(), provider_meta.clone(), attachments))
                        }
                        _ => None,
                    });
                    let (name, input, provider_meta, attachments) = existing.unwrap_or((
                        part.get("tool").and_then(Value::as_str).unwrap_or_default().to_string(),
                        part.pointer("/state/input").cloned().unwrap_or(Value::Null),
                        None,
                        Vec::new(),
                    ));
                    let state = part.get("state").cloned().unwrap_or(Value::Null);
                    let state = match state.get("status").and_then(Value::as_str) {
                        Some("completed") => ToolState::Completed {
                            output: state.get("output").and_then(Value::as_str).unwrap_or_default().to_string(),
                            title: state.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
                            metadata: state.get("metadata").cloned().unwrap_or(Value::Null),
                            attachments: state
                                .get("attachments")
                                .and_then(Value::as_array)
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| {
                                            let mime = item.get("mime").and_then(Value::as_str)?.to_string();
                                            let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                                            let data = url.split_once("base64,").map(|(_, data)| data.to_string()).unwrap_or_default();
                                            if data.is_empty() {
                                                return None;
                                            }
                                            Some(crate::session::FilePart {
                                                mime,
                                                data,
                                                name: item.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or(attachments),
                        },
                        Some("error") => ToolState::Error {
                            error: state.get("error").and_then(Value::as_str).unwrap_or_default().to_string(),
                        },
                        Some("running") => ToolState::Running,
                        _ => ToolState::Pending,
                    };
                    next.push(Part::Tool { id: call.to_string(), name, input, state, provider_meta });
                }
                Some("file") => {
                    let mime = part.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
                    let url = part.get("url").and_then(Value::as_str).unwrap_or_default();
                    let data = url.split_once("base64,").map(|(_, data)| data.to_string()).unwrap_or_default();
                    next.push(Part::File(crate::session::FilePart {
                        mime,
                        data,
                        name: part.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                    }));
                }
                Some("compaction") => next.push(Part::Compaction {
                    summary: part.get("summary").and_then(Value::as_str).unwrap_or_default().to_string(),
                    metadata: part.get("metadata").cloned().filter(|value| !value.is_null()),
                }),
                _ => {}
            }
        }
        message.parts = next;
        result.push(message);
    }
    // A transform may add, remove or reorder messages, so the returned list is
    // authoritative. The caller passes a request-level clone, never the stored
    // session history.
    *messages = result;
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default()
}

fn model_from_value(provider_id: &str, value: &Value, base: Option<&crate::models::Model>) -> Option<crate::models::Model> {
    let id = value.get("id").and_then(Value::as_str)?;
    let cost = value.get("cost").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
    let limit = value.get("limit").cloned().unwrap_or(Value::Null);
    let caps = value.get("capabilities").cloned().unwrap_or(Value::Null);
    Some(crate::models::Model {
        id: id.to_string(),
        provider_id: provider_id.to_string(),
        kind: crate::models::ModelKind::Chat,
        name: value.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
        family: value.get("family").and_then(Value::as_str).unwrap_or_default().to_string(),
        reasoning: base.map(|m| m.reasoning).unwrap_or(false),
        reasoning_options: base.map(|m| m.reasoning_options.clone()).unwrap_or_default(),
        tool_call: caps.get("tools").and_then(Value::as_bool).unwrap_or(true),
        attachment: caps
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|m| m.as_str() == Some("image"))),
        temperature: caps.get("temperature").and_then(Value::as_bool).unwrap_or(true),
        context: limit.get("context").and_then(Value::as_u64).unwrap_or(0),
        output: limit.get("output").and_then(Value::as_u64).unwrap_or(0),
        cost_input: cost.get("input").and_then(Value::as_f64).unwrap_or(0.0),
        cost_output: cost.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        cost_cache_read: cost.pointer("/cache/read").and_then(Value::as_f64).unwrap_or(0.0),
        cost_cache_write: cost.pointer("/cache/write").and_then(Value::as_f64).unwrap_or(0.0),
        input_modalities: caps
            .get("input")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_else(|| vec!["text".into()]),
        output_modalities: vec!["text".into()],
        interleaved_field: base.and_then(|m| m.interleaved_field.clone()),
        release_date: base.map(|m| m.release_date.clone()).unwrap_or_default(),
        status: value.get("status").and_then(Value::as_str).map(String::from),
        npm: base.and_then(|m| m.npm.clone()),
        api: base.and_then(|m| m.api.clone()),
        shape: base.and_then(|m| m.shape.clone()),
        options: value.get("body").cloned().unwrap_or(Value::Null),
        headers: string_map(value.get("headers")),
    })
}
