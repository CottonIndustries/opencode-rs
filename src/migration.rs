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

pub fn status() -> Value {
    let legacy = legacy_sessions();
    let imported = crate::db::total();
    if legacy == 0 || imported > 0 {
        json!({ "status": "completed" })
    } else {
        json!({ "status": "required" })
    }
}
