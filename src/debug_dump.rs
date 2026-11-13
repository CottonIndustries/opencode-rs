//! Diagnostic dumps for `POST /api/debug/heap-dump`.
//!
//! Rust has no portable heap-snapshot API, so this writes a structured process
//! and registry dump that is useful for support without exposing credentials.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn dump(app: &crate::app::App) -> Result<Value> {
    let directory: PathBuf = crate::paths::get().storage().join("debug");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("heap-{}.json", crate::util::now_ms()));
    let sessions = app.session_statuses();
    let value = json!({
        "version": crate::VERSION,
        "pid": std::process::id(),
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "directory": app.directory.to_string_lossy(),
        "worktree": app.worktree.to_string_lossy(),
        "project": app.project,
        "config": app.config(),
        "providers": app.providers().keys().cloned().collect::<Vec<_>>(),
        "agents": app.agents().keys().cloned().collect::<Vec<_>>(),
        "sessions": sessions
            .iter()
            .map(|(id, busy, retry)| json!({ "id": id, "busy": busy, "retry": retry }))
            .collect::<Vec<_>>(),
        "jobs": crate::job::list(None, None, None, false),
        "savedPermissions": crate::permission_saved::list(Some(&app.project)),
    });
    let bytes = serde_json::to_vec_pretty(&value)?;
    std::fs::write(&path, &bytes)?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "bytes": bytes.len(),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn debug_path_is_stable() {
        assert!(crate::paths::get().storage().join("debug").ends_with("debug"));
    }
}
