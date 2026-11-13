//! `@path` and `@skill` mention expansion, shared by the TUI and CLI.

use crate::app::App;
use crate::session::{FilePart, Part};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::Path;

/// Build message parts for an attached file or directory.
pub fn attach(path: &Path, display: &str) -> Result<Vec<Part>> {
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)?
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        return Ok(vec![Part::Text {
            text: format!("\n\nAttached directory: {}\n\n{}", path.display(), names.join("\n")),
            synthetic: true,
        }]);
    }
    let bytes = std::fs::read(path).map_err(|error| anyhow!("cannot read {display}: {error}"))?;
    let mime = mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string();
    if mime.starts_with("image/") || mime == "application/pdf" {
        return Ok(vec![
            Part::Text { text: format!("Attached file: {}", path.display()), synthetic: true },
            Part::File(FilePart {
                mime,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                name: display.to_string(),
            }),
        ]);
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(vec![Part::Text {
        text: format!("\n\nAttached file: {}\n\n{}", path.display(), text),
        synthetic: true,
    }])
}

fn mention_ids(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(?:^|\s)@([^\s]+)").unwrap();
    re.captures_iter(text)
        .map(|capture| capture[1].trim_end_matches([',', '.', ')', ';', ':']).to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Skill attachments selected with `@id` for the V2 message shape.
pub fn skill_mentions(app: &App, text: &str) -> Option<Value> {
    let skills = app.skills();
    let mut out: Vec<Value> = Vec::new();
    for id in mention_ids(text) {
        let Some(skill) = skills.iter().find(|skill| skill.id == id) else {
            continue;
        };
        if out.iter().any(|item| item.get("id").and_then(Value::as_str) == Some(skill.id.as_str())) {
            continue;
        }
        out.push(json!({ "id": skill.id, "name": skill.name }));
    }
    (!out.is_empty()).then(|| Value::Array(out))
}

/// Expand `@path` and `@skill` mentions into message parts.
pub fn expand_mentions(app: &App, text: &str) -> Vec<Part> {
    let skills = app.skills();
    let mut selected = std::collections::HashSet::new();
    let mut parts = Vec::new();
    for raw in mention_ids(text) {
        let path = crate::util::resolve(&app.directory, &raw);
        if path.exists() {
            if let Ok(mut attached) = attach(&path, &raw) {
                parts.append(&mut attached);
            }
        } else if let Some(skill) = skills.iter().find(|skill| skill.id == raw) {
            if selected.insert(skill.id.clone()) {
                parts.push(Part::Text {
                    text: crate::tools::skill::render(skill),
                    synthetic: true,
                });
            }
        }
    }
    parts
}
