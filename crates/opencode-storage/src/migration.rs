//! V1 session-history migration status.

use serde_json::{json, Value};

fn legacy_sessions() -> usize {
    let root = crate::paths::get().storage().join("session");
    let Ok(projects) = std::fs::read_dir(root) else { return 0 };
    projects
        .flatten()
        .map(|project| {
            std::fs::read_dir(project.path())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// Index every legacy session file into the SQLite store.
pub fn run() -> Value {
    let mut migrated = 0usize;
    let mut failed = 0usize;
    let root = crate::paths::get().storage().join("session");
    if let Ok(projects) = std::fs::read_dir(root) {
        for project in projects.flatten() {
            let Ok(entries) = std::fs::read_dir(project.path()) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { failed += 1; continue };
                let Ok(session) = serde_json::from_str::<crate::session::Session>(&text) else {
                    failed += 1;
                    continue;
                };
                match crate::session::save(&session) {
                    Ok(()) => migrated += 1,
                    Err(_) => failed += 1,
                }
            }
        }
    }
    json!({ "migrated": migrated, "failed": failed, "status": status()["status"].clone() })
}

pub fn status() -> Value {
    let legacy = legacy_sessions();
    let imported = crate::db::total();
    if legacy == 0 || imported > 0 {
        json!({ "status": "completed" })
    } else {
        json!({ "status": "required" })
    }
}
