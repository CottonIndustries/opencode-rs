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
            CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project, updated DESC);
            CREATE TABLE IF NOT EXISTS messages (
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                role TEXT NOT NULL,
                created INTEGER NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY (session_id, message_id)
            );
            CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, ordinal);",
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

pub fn save_messages(session_id: &str, messages: &[serde_json::Value]) {
    let Some(db) = connection() else { return };
    let Ok(mut db) = db.lock() else { return };
    let Ok(transaction) = db.transaction() else { return };
    if transaction.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id]).is_err() {
        return;
    }
    for (ordinal, message) in messages.iter().enumerate() {
        let Some(id) = message.get("id").and_then(serde_json::Value::as_str) else { continue };
        let role = message.get("role").and_then(serde_json::Value::as_str).unwrap_or("user");
        let created = message.get("time").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let data = message.to_string();
        let _ = transaction.execute(
            "INSERT INTO messages(session_id, message_id, ordinal, role, created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, id, ordinal as i64, role, created, data],
        );
    }
    let _ = transaction.commit();
}

pub fn message_contains(text: &str) -> bool {
    let Some(db) = connection() else { return false };
    let Ok(db) = db.lock() else { return false };
    db.query_row(
        "SELECT 1 FROM messages WHERE data LIKE '%' || ?1 || '%' LIMIT 1",
        params![text],
        |_| Ok(()),
    )
    .is_ok()
}

pub fn messages(session_id: &str) -> Vec<String> {
    let Some(db) = connection() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let Ok(mut statement) = db.prepare("SELECT data FROM messages WHERE session_id = ?1 ORDER BY ordinal") else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map(params![session_id], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

pub fn load(id: &str) -> Option<String> {
    let db = connection()?;
    let db = db.lock().ok()?;
    db.query_row("SELECT data FROM sessions WHERE id = ?1", params![id], |row| row.get(0)).ok()
}

pub fn updated(id: &str) -> Option<i64> {
    let db = connection()?;
    let db = db.lock().ok()?;
    db.query_row("SELECT updated FROM sessions WHERE id = ?1", params![id], |row| row.get(0)).ok()
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
    let _ = db.execute("DELETE FROM messages WHERE session_id = ?1", params![id]);
    db.execute("DELETE FROM sessions WHERE id = ?1", params![id]).is_ok_and(|rows| rows > 0)
}

pub fn contains(text: &str) -> bool {
    let Some(db) = connection() else { return false };
    let Ok(db) = db.lock() else { return false };
    db.query_row(
        "SELECT 1 FROM sessions WHERE data LIKE '%' || ?1 || '%' LIMIT 1",
        params![text],
        |_| Ok(()),
    )
    .is_ok()
}

pub fn total() -> i64 {
    let Some(db) = connection() else { return 0 };
    let Ok(db) = db.lock() else { return 0 };
    db.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0)).unwrap_or(0)
}

pub fn count(project: &str) -> i64 {
    let Some(db) = connection() else { return 0 };
    let Ok(db) = db.lock() else { return 0 };
    db.query_row("SELECT COUNT(*) FROM sessions WHERE project = ?1", params![project], |row| row.get(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip() {
        let session = crate::util::id("ses_test");
        let items = vec![
            json!({ "id": "msg_one", "role": "user", "time": 1, "parts": [] }),
            json!({ "id": "msg_two", "role": "assistant", "time": 2, "parts": [] }),
        ];
        save_messages(&session, &items);
        let stored = messages(&session);
        assert_eq!(stored.len(), 2);
        assert!(stored[0].contains("msg_one"));
        assert!(stored[1].contains("msg_two"));
        delete(&session);
        assert!(messages(&session).is_empty());
    }
}
