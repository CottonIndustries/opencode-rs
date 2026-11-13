//! Saved permission rules.
//!
//! A saved rule is the durable form of "always allow" for one action and
//! resource inside a project. Rules live in `storage/permissions.db` and are
//! evaluated after configured, agent and session rules.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct Saved {
    pub id: String,
    pub project_id: String,
    pub action: String,
    pub resource: String,
    pub created: i64,
    pub updated: i64,
}

fn db() -> Option<&'static Mutex<Connection>> {
    static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
    if let Some(db) = DB.get() {
        return Some(db);
    }
    let path: PathBuf = crate::paths::get().storage().join("permissions.db");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let connection = Connection::open(&path).ok()?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS permission_saved (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                action TEXT NOT NULL,
                resource TEXT NOT NULL,
                created INTEGER NOT NULL,
                updated INTEGER NOT NULL,
                UNIQUE(project_id, action, resource)
             );
             CREATE INDEX IF NOT EXISTS permission_saved_project ON permission_saved(project_id, created);",
        )
        .ok()?;
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Saved> {
    Ok(Saved {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        action: row.get("action")?,
        resource: row.get("resource")?,
        created: row.get("created")?,
        updated: row.get("updated")?,
    })
}

pub fn info(saved: &Saved) -> Value {
    json!({
        "id": saved.id,
        "projectID": saved.project_id,
        "action": saved.action,
        "resource": saved.resource,
        "time": {
            "created": saved.created,
            "updated": saved.updated,
        },
    })
}

pub fn list(project: Option<&str>) -> Vec<Value> {
    let Some(db) = db() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(project) = project {
        let Ok(mut statement) = db.prepare("SELECT * FROM permission_saved WHERE project_id = ?1 ORDER BY created, id") else {
            return out;
        };
        let Ok(rows) = statement.query_map(params![project], row) else { return out };
        out.extend(rows.flatten().map(|saved| info(&saved)));
    } else {
        let Ok(mut statement) = db.prepare("SELECT * FROM permission_saved ORDER BY created, id") else {
            return out;
        };
        let Ok(rows) = statement.query_map([], row) else { return out };
        out.extend(rows.flatten().map(|saved| info(&saved)));
    }
    out
}

pub fn rules(project: &str) -> Vec<(String, String)> {
    let Some(db) = db() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let Ok(mut statement) = db.prepare("SELECT action, resource FROM permission_saved WHERE project_id = ?1") else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map(params![project], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// Save rules for an "always" permission reply. Duplicates are ignored.
pub fn add(project: &str, action: &str, resources: &[String]) -> Result<Vec<Value>> {
    if resources.is_empty() {
        return Ok(Vec::new());
    }
    let Some(db) = db() else { return Err(anyhow!("permission database unavailable")) };
    let db = db.lock().map_err(|_| anyhow!("permission database unavailable"))?;
    let now = crate::util::now_ms();
    let mut out = Vec::new();
    for resource in resources {
        let id = crate::util::id("psv");
        let inserted = db.execute(
            "INSERT OR IGNORE INTO permission_saved(id, project_id, action, resource, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, project, action, resource, now],
        )?;
        if inserted > 0 {
            out.push(info(&Saved {
                id,
                project_id: project.to_string(),
                action: action.to_string(),
                resource: resource.clone(),
                created: now,
                updated: now,
            }));
        }
    }
    Ok(out)
}

pub fn update(id: &str, action: Option<&str>, resource: Option<&str>) -> Result<Value> {
    let Some(db) = db() else { return Err(anyhow!("permission database unavailable")) };
    let db = db.lock().map_err(|_| anyhow!("permission database unavailable"))?;
    let current = db
        .query_row("SELECT * FROM permission_saved WHERE id = ?1", params![id], row)
        .map_err(|_| anyhow!("saved permission not found: {id}"))?;
    let action = action.unwrap_or(&current.action);
    let resource = resource.unwrap_or(&current.resource);
    db.execute(
        "UPDATE permission_saved SET action = ?2, resource = ?3, updated = ?4 WHERE id = ?1",
        params![id, action, resource, crate::util::now_ms()],
    )
    .map_err(|error| {
        if error.to_string().contains("UNIQUE") {
            anyhow!("a saved permission already exists for {action} {resource}")
        } else {
            error.into()
        }
    })?;
    let saved = db.query_row("SELECT * FROM permission_saved WHERE id = ?1", params![id], row)?;
    Ok(info(&saved))
}

pub fn remove(id: &str) -> Result<bool> {
    let Some(db) = db() else { return Ok(false) };
    let db = db.lock().map_err(|_| anyhow!("permission database unavailable"))?;
    Ok(db.execute("DELETE FROM permission_saved WHERE id = ?1", params![id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_updates_rules() {
        let project = format!("project_{}", crate::util::id("test"));
        let saved = add(&project, "edit", &["src/main.rs".into()]).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(add(&project, "edit", &["src/main.rs".into()]).unwrap().len(), 0);
        let id = saved[0]["id"].as_str().unwrap();
        let updated = update(id, None, Some("src/lib.rs")).unwrap();
        assert_eq!(updated["resource"], "src/lib.rs");
        assert_eq!(rules(&project), vec![("edit".to_string(), "src/lib.rs".to_string())]);
        assert!(remove(id).unwrap());
        assert!(rules(&project).is_empty());
    }
}
