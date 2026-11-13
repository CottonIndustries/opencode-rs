//! SQLite index for session documents.
//!
//! Session transcripts still serialize to JSON so existing tools can inspect
//! them, but the database is the source of truth for loading and listing. The
//! first list for a project imports any JSON files that predate the database.

use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

fn path() -> PathBuf {
    crate::paths::get().storage().join("sessions.db")
}

pub fn connection() -> Option<&'static Mutex<Connection>> {
    if let Some(db) = DB.get() {
        return Some(db);
    }
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let connection = Connection::open(&path).ok()?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                project TEXT NOT NULL,
                updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project, updated DESC);",
        )
        .ok()?;
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

pub fn save(id: &str, project: &str, updated: i64, data: &str) {
    let Some(db) = connection() else { return };
    let Ok(db) = db.lock() else { return };
    let _ = db.execute(
        "INSERT INTO sessions(id, project, updated, data) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET project = excluded.project, updated = excluded.updated, data = excluded.data",
        params![id, project, updated, data],
    );
}

pub fn load(id: &str) -> Option<String> {
    let db = connection()?;
    let db = db.lock().ok()?;
    db.query_row("SELECT data FROM sessions WHERE id = ?1", params![id], |row| row.get(0)).ok()
}

pub fn list(project: &str) -> Vec<String> {
    let Some(db) = connection() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let Ok(mut statement) = db.prepare("SELECT data FROM sessions WHERE project = ?1 ORDER BY updated DESC") else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map(params![project], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

pub fn delete(id: &str) -> bool {
    let Some(db) = connection() else { return false };
    let Ok(db) = db.lock() else { return false };
    db.execute("DELETE FROM sessions WHERE id = ?1", params![id]).is_ok_and(|rows| rows > 0)
}

pub fn count(project: &str) -> i64 {
    let Some(db) = connection() else { return 0 };
    let Ok(db) = db.lock() else { return 0 };
    db.query_row("SELECT COUNT(*) FROM sessions WHERE project = ?1", params![project], |row| row.get(0))
        .unwrap_or(0)
}
