//! Helpers behind `/open`, `/cd` and `/worktrees`.
//!
//! The UI layer only calls these to build dialog options and to persist a
//! location change; rendering and key handling stay in `tui::mod`.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Sessions across every project, grouped by project directory name.
///
/// `App` carries no active-session id, so the current marker falls back to the
/// most recently updated session of the current project (then of all projects).
pub fn open_options(_app: &crate::app::App, current_session: Option<&str>) -> Vec<crate::tui::dialog::Opt> {
    let root = crate::paths::get().storage().join("session");
    let mut entries: Vec<(String, crate::session::Summary)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for entry in rd.flatten() {
            let Some(project) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            entries.extend(crate::session::list(&project).into_iter().map(|session| (project.clone(), session)));
        }
    }
    entries.retain(|(_, session)| session.parent_id.is_none());

    let current = current_session.map(String::from).or_else(|| {
        entries
            .iter()
            .max_by_key(|(_, session)| session.time_updated)
            .map(|(_, session)| session.id.clone())
    });

    entries.sort_by(|(_, a), (_, b)| {
        project_name(a)
            .to_lowercase()
            .cmp(&project_name(b).to_lowercase())
            .then(b.time_updated.cmp(&a.time_updated))
    });

    entries
        .into_iter()
        .map(|(project_id, session)| {
            let project = project_name(&session);
            let footer = format!("{project} - {}", ago(session.time_updated));
            crate::tui::dialog::Opt {
                title: session.title.clone(),
                footer,
                category: project.clone(),
                value: format!("{project_id}|{}", session.id),
                search: format!("{} {} {}", session.title, project, session.directory.display()),
                current: current.as_deref() == Some(session.id.as_str()),
                ..Default::default()
            }
        })
        .collect()
}

/// Move a session to `directory` (which may start with `~`), record the switch
/// and persist it. The old terminal is stopped; its WebSocket transport is only
/// closable asynchronously, so a caller that cares must do that itself.
pub fn move_session(handle: &crate::app::Handle, directory: &Path) -> Result<()> {
    let id = handle.id();
    let base = handle.with(|session| session.directory.clone());
    let target = resolve(directory, &base);
    let target = std::fs::canonicalize(&target)
        .map(|path| crate::app::strip_unc(&path))
        .unwrap_or(target);
    if !target.is_dir() {
        return Err(anyhow!("not a directory: {}", target.display()));
    }
    crate::plugin::kill_terminal(&id);
    handle.with(|session| {
        let previous = session.directory.clone();
        session.record(serde_json::json!({
            "type": "location-switched",
            "location": { "directory": target.to_string_lossy() },
            "projectID": crate::session::project_id(&target),
            "previous": { "location": { "directory": previous.to_string_lossy() } },
        }));
        session.directory = target.clone();
        session.project = crate::session::project_id(&target);
    });
    handle.save();
    Ok(())
}

/// Workspaces from the durable registry, newest used first.
pub fn workspace_options() -> Vec<crate::tui::dialog::Opt> {
    crate::workspace::list()
        .into_iter()
        .map(|workspace| {
            let directory = Path::new(&workspace.directory);
            let name = directory
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| workspace.directory.clone());
            crate::tui::dialog::Opt {
                title: name,
                description: workspace.directory.clone(),
                footer: format!("{} · {}", workspace.provider, workspace.id),
                value: workspace.id.clone(),
                search: format!("{} {} {}", workspace.id, workspace.provider, workspace.directory),
                ..Default::default()
            }
        })
        .collect()
}

/// Bind the session to an existing workspace and persist the new location.
pub fn bind_workspace(handle: &crate::app::Handle, workspace_id: &str) -> Result<()> {
    let environment = crate::workspace::connect(workspace_id)?;
    let directory = environment.root().to_path_buf();
    let id = handle.id();
    crate::plugin::kill_terminal(&id);
    handle.with(|session| {
        let previous = session.directory.clone();
        session.record(serde_json::json!({
            "type": "location-switched",
            "location": { "directory": directory.to_string_lossy() },
            "projectID": crate::session::project_id(&directory),
            "workspaceID": workspace_id,
            "previous": { "location": { "directory": previous.to_string_lossy() } },
        }));
        session.workspace_id = Some(workspace_id.to_string());
        session.directory = directory.clone();
        session.project = crate::session::project_id(&directory);
    });
    handle.save();
    Ok(())
}

/// Create a local workspace rooted at `directory` and return its id.
pub fn create_workspace(directory: &Path, base: &Path) -> Result<String> {
    let directory = resolve(directory, base);
    Ok(crate::workspace::create(None, "local", &directory)?.id)
}

/// Expand a leading `~` and resolve a relative path against `base`.
fn resolve(directory: &Path, base: &Path) -> PathBuf {
    let text = directory.to_string_lossy();
    let expanded = if text == "~" {
        crate::paths::get().home.clone()
    } else if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        crate::paths::get().home.join(rest)
    } else {
        directory.to_path_buf()
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn project_name(session: &crate::session::Summary) -> String {
    session
        .directory
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| session.directory.display().to_string())
}

fn ago(ms: i64) -> String {
    let seconds = (crate::util::now_ms() - ms) / 1000;
    match seconds {
        seconds if seconds < 60 => format!("{seconds}s ago"),
        seconds if seconds < 3600 => format!("{}m ago", seconds / 60),
        seconds if seconds < 86400 => format!("{}h ago", seconds / 3600),
        seconds => format!("{}d ago", seconds / 86400),
    }
}
