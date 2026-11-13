//! Skills: `SKILL.md` folders under `skill/` or `skills/` in config directories,
//! plus built-ins, and the `skill` tool that loads them.

use super::{req_str, Context, Output, Tool};
use crate::config::Config;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const OPENCODE_SKILL: &str = include_str!("../../assets/prompts/opencode.md");
const REPORT_SKILL: &str = include_str!("../../assets/prompts/report.md");

#[derive(Debug, Clone)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
    /// Whether the skill appears in the model's available-skills guidance.
    pub autoinvoke: bool,
}

fn report_content(plugins: &[crate::plugin::PluginSpec]) -> String {
    let terminal = ["TERM_PROGRAM", "TERM", "COLORTERM"]
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>();
    let terminal = if terminal.is_empty() {
        "Unavailable: terminal environment variables are not set".to_string()
    } else {
        terminal.join(", ")
    };
    let shell = std::env::var("SHELL")
        .or_else(|_| std::env::var("ComSpec"))
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_else(|_| "Unavailable: shell environment variable is not set".into());
    let mut plugins = plugins.iter().map(|plugin| plugin.target.clone()).collect::<Vec<_>>();
    plugins.sort();
    plugins.dedup();
    let plugins = if plugins.is_empty() {
        "None found in config".to_string()
    } else {
        plugins.join(", ")
    };
    format!(
        "{}

## Runtime Diagnostics Snapshot

These values were captured when the built-in report skill was registered. Verify them before publishing.

- opencode version: {}
- install/channel: {}
- OS: {} ({})
- Terminal: {}
- Shell: {}
- Active plugins: {}
",
        REPORT_SKILL.trim_end(),
        crate::UPSTREAM_VERSION,
        crate::UPSTREAM_CHANNEL,
        std::env::consts::OS,
        std::env::consts::ARCH,
        terminal,
        shell,
        plugins,
    )
}

fn builtins(plugins: &[crate::plugin::PluginSpec]) -> Vec<Skill> {
    vec![
        Skill {
            id: "opencode".into(),
            name: "OpenCode".into(),
            description: "Use this skill for any question about OpenCode itself, including how OpenCode works, using or configuring it, migrating from V1 to V2, troubleshooting it, developing plugins or integrations, using the OpenCode SDK, clients, server, or API, and contributing to the OpenCode codebase. Also use it for OpenCode agents, commands, skills, tools, permissions, MCP servers, providers, models, themes, keybinds, formatters, the CLI, TUI, desktop app, and web app.".into(),
            path: PathBuf::from("/builtin/opencode.md"),
            content: OPENCODE_SKILL.into(),
            autoinvoke: true,
        },
        Skill {
            id: "report".into(),
            name: "Report".into(),
            description: "Use when the user wants to report an opencode issue or bug. Collect standard diagnostics, add user-specific reproduction context, and publish the issue with GitHub CLI.".into(),
            path: PathBuf::from("/builtin/report.md"),
            content: report_content(plugins),
            autoinvoke: true,
        },
    ]
}

fn add_skill(path: &Path, out: &mut Vec<Skill>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let (fm, body) = crate::config::frontmatter(&text);
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let id = if file_name == "SKILL.md" {
        path.parent()
            .and_then(|dir| dir.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        path.file_stem()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    if id.is_empty() {
        return;
    }
    let name = fm.get("name").and_then(|value| value.as_str()).map(String::from).unwrap_or_else(|| id.clone());
    let description = fm.get("description").and_then(|value| value.as_str()).unwrap_or("").to_string();
    if out.iter().any(|skill| skill.id == id) {
        out.retain(|skill| skill.id != id);
    }
    let metadata_bool = |value: &Value| match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_i64().and_then(|value| match value {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let metadata = fm.get("metadata").and_then(Value::as_object);
    let autoinvoke = metadata
        .and_then(|metadata| metadata.get("opencode/autoinvoke"))
        .and_then(metadata_bool)
        .or_else(|| fm.get("disable-model-invocation").and_then(metadata_bool).map(|value| !value))
        .unwrap_or(true);
    out.push(Skill { id, name, description, path: path.to_path_buf(), content: body, autoinvoke });
}

fn is_ignored_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(name, "node_modules" | ".venv" | "venv" | "__pycache__")
}

fn scan_dir(dir: &Path, out: &mut Vec<Skill>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<std::fs::DirEntry> = read.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    if let Some(skill) = entries
        .iter()
        .find(|entry| entry.file_name().to_string_lossy() == "SKILL.md" && entry.path().is_file())
    {
        add_skill(&skill.path(), out);
        return;
    }
    if depth == 0 {
        for entry in &entries {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                add_skill(&path, out);
            }
        }
    }
    for entry in entries {
        let path = entry.path();
        if path.is_dir()
            && !is_ignored_dir(&entry.file_name().to_string_lossy())
        {
            scan_dir(&path, out, depth + 1);
        }
    }
}

fn scan(path: &Path, out: &mut Vec<Skill>) {
    if path.is_file() {
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            add_skill(path, out);
        }
    } else if path.is_dir() {
        scan_dir(path, out, 0);
    }
}

fn remote_cache_dir(url: &str) -> Option<PathBuf> {
    let base = url.trim();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return None;
    }
    let base = if base.ends_with('/') { base.to_string() } else { format!("{base}/") };
    let hash = format!("{:x}", Sha256::digest(base.as_bytes()));
    Some(crate::paths::get().cache.join("skills").join(hash))
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains(char::from(92))
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(char::from(92))
        && !value.contains('?')
        && !value.contains('#')
        && !value.contains(':')
        && !value.starts_with('/')
        && !value.split('/').any(|part| part.is_empty() || part == "." || part == "..")
}

/// Download URL skill sources into the cache, matching upstream's index format.
pub async fn sync_remote(config: &Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for url in &config.skills {
        let Some(root) = remote_cache_dir(url) else { continue };
        let base = if url.ends_with('/') { url.clone() } else { format!("{url}/") };
        let index_url = format!("{base}index.json");
        let response = crate::http::send(
            crate::http::client()
                .get(&index_url)
                .timeout(std::time::Duration::from_secs(15)),
        )
        .await;
        let Ok(response) = response else { continue };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text().await else { continue };
        let Ok(index) = serde_json::from_str::<Value>(&body) else { continue };
        let Some(skills) = index.get("skills").and_then(Value::as_array) else { continue };
        for skill in skills {
            let Some(name) = skill.get("name").and_then(Value::as_str) else { continue };
            if !safe_segment(name) {
                continue;
            }
            let Some(files) = skill.get("files").and_then(Value::as_array) else { continue };
            let names: Vec<&str> = files.iter().filter_map(Value::as_str).collect();
            if !names.iter().any(|file| *file == "SKILL.md" || *file == format!("{name}.md")) {
                continue;
            }
            let directory = root.join(name);
            let version = skill.get("version").and_then(Value::as_str);
            let version_file = directory.join(".opencode-version");
            let current = std::fs::read_to_string(&version_file).ok();
            if version.is_some() && current.as_deref() != version {
                let _ = std::fs::remove_dir_all(&directory);
            }
            let _ = std::fs::create_dir_all(&directory);
            for file in names {
                if !safe_relative(file) {
                    continue;
                }
                let destination = directory.join(file);
                if destination.is_file() {
                    continue;
                }
                let Ok(response) = crate::http::send(
                    crate::http::client()
                        .get(format!("{base}{}/{}", name, file))
                        .timeout(std::time::Duration::from_secs(30)),
                )
                .await
                else {
                    continue;
                };
                if !response.status().is_success() {
                    continue;
                }
                let Ok(bytes) = response.bytes().await else { continue };
                if let Some(parent) = destination.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(destination, bytes);
            }
            if directory.join("SKILL.md").is_file() || directory.join(format!("{name}.md")).is_file() {
                if let Some(version) = version {
                    let _ = std::fs::write(&version_file, version);
                }
                out.push(directory);
            }
        }
    }
    out
}

pub fn discover(
    config: &Config,
    dirs: &[PathBuf],
    directory: &Path,
    plugins: &[crate::plugin::PluginSpec],
) -> Vec<Skill> {
    let mut out = builtins(plugins);
    // Like upstream: global `~/.claude` and `~/.agents`, then the same folders up the project tree.
    let home = &crate::paths::get().home;
    let mut shared = vec![home.join(".claude"), home.join(".agents")];
    let mut ancestors: Vec<&Path> = directory.ancestors().take_while(|a| *a != home.as_path()).collect();
    ancestors.reverse();
    for a in ancestors {
        shared.push(a.join(".claude"));
        shared.push(a.join(".agents"));
    }
    for d in &shared {
        scan(&d.join("skill"), &mut out);
        scan(&d.join("skills"), &mut out);
    }
    for d in dirs {
        scan(&d.join("skill"), &mut out);
        scan(&d.join("skills"), &mut out);
    }
    scan(&crate::console::skills_dir(), &mut out);
    for s in &config.skills {
        if s.starts_with("http://") || s.starts_with("https://") {
            if let Some(cache) = remote_cache_dir(s) {
                scan(&cache, &mut out);
            }
            continue;
        }
        scan(&crate::util::resolve(directory, s), &mut out);
    }
    out
}

pub fn render(skill: &Skill) -> String {
    let dir = skill.path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let mut files: Vec<String> = Vec::new();
    if skill.path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
        for e in ignore::WalkBuilder::new(&dir).hidden(false).build().flatten() {
            if e.file_type().is_some_and(|t| t.is_file()) && e.path().file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                files.push(e.path().display().to_string());
            }
        }
        files.sort();
        files.truncate(10);
    }
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.content.trim().to_string(),
        String::new(),
        format!("Base directory for this skill: {}", dir.display()),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    lines.extend(files.iter().map(|f| format!("<file>{f}</file>")));
    lines.push("</skill_files>".into());
    lines.push("</skill_content>".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_skill_directories_and_root_markdown() {
        let root = std::env::temp_dir().join(format!("opencode-skills-{}", crate::util::id("")));
        let alpha = root.join("alpha");
        let gamma = root.join("gamma");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&gamma).unwrap();
        std::fs::write(
            alpha.join("SKILL.md"),
            "---\nname: Alpha\ndescription: first\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            gamma.join("SKILL.md"),
            "---\nname: Gamma\nmetadata:\n  opencode/autoinvoke: false\n---\nbody",
        )
        .unwrap();
        std::fs::write(root.join("beta.md"), "---\ndescription: root file\n---\nbody").unwrap();
        let mut skills = Vec::new();
        scan(&root, &mut skills);
        let ids: Vec<&str> = skills.iter().map(|skill| skill.id.as_str()).collect();
        assert!(ids.contains(&"alpha"));
        assert!(ids.contains(&"beta"));
        assert!(ids.contains(&"gamma"));
        assert_eq!(skills.iter().find(|skill| skill.id == "beta").unwrap().name, "beta");
        assert!(!skills.iter().find(|skill| skill.id == "gamma").unwrap().autoinvoke);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn report_builtin_includes_runtime_diagnostics() {
        let content = report_content(&[]);
        assert!(content.contains("## Runtime Diagnostics Snapshot"));
        assert!(content.contains(crate::UPSTREAM_VERSION));
        assert!(content.contains("Active plugins: None found in config"));
    }
}

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> String {
        "Load a specialized skill's instructions and resources into the current conversation when the task at hand matches its description.\n\nThe skill ID must match an available skill or a skill explicitly referenced by the user.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "The ID of an available skill or a skill explicitly referenced by the user" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id").or_else(|_| req_str(&input, "name"))?;
        let skills = ctx.app.skills();
        let skill = skills
            .iter()
            .find(|s| s.id == id || s.name.eq_ignore_ascii_case(id))
            .ok_or_else(|| {
                let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
                anyhow!("Unable to load skill {id}: not found. Available skills: {}", ids.join(", "))
            })?;
        ctx.ask("skill", &[skill.id.clone()], &[skill.id.clone()], format!("Load skill {}", skill.name), json!({ "name": skill.name })).await?;
        Ok(Output::text(render(skill), skill.name.clone()).meta(json!({ "name": skill.name, "directory": skill.path.parent().map(|p| p.display().to_string()) })))
    }
}
