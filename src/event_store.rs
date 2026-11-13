//! Append-only durable event log.
//!
//! Events are stored in SQLite so `session.log` can page a session's history
//! by sequence without scanning a file. The legacy JSONL log is imported once
//! when the database is first created, so existing installations keep their
//! replayable history.

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

fn path() -> PathBuf {
    crate::paths::get().storage().join("events.db")
}

fn legacy_path() -> PathBuf {
    crate::paths::get().storage().join("events.jsonl")
}

fn open() -> Option<&'static Mutex<Connection>> {
    if let Some(db) = DB.get() {
        return Some(db);
    }
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut connection = Connection::open(&path).ok()?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY,
                session_id TEXT,
                type TEXT NOT NULL,
                created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS events_session ON events(session_id, seq);
            CREATE INDEX IF NOT EXISTS events_type ON events(type, seq);
            UPDATE events
               SET session_id = COALESCE(json_extract(data, '$.properties.sessionID'), json_extract(data, '$.properties.info.id'))
             WHERE (session_id IS NULL OR session_id = '')
               AND json_extract(data, '$.properties.info.id') IS NOT NULL;",
        )
        .ok()?;
    migrate(&mut connection);
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

/// Import the old JSONL log once, preserving sequence numbers where possible.
fn migrate(connection: &mut Connection) {
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap_or(0);
    if count > 0 {
        return;
    }
    let Ok(file) = std::fs::File::open(legacy_path()) else { return };
    let Ok(transaction) = connection.transaction() else { return };
    let mut next = 1i64;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
        let seq = event.get("seq").and_then(Value::as_u64).map(|seq| seq as i64).unwrap_or(next);
        next = next.max(seq + 1);
        let session = event
            .pointer("/properties/sessionID")
            .or_else(|| event.pointer("/properties/info/id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ty = event.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let created = event.get("created").and_then(Value::as_i64).unwrap_or_else(crate::util::now_ms);
        let _ = transaction.execute(
            "INSERT OR IGNORE INTO events(seq, session_id, type, created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq, session, ty, created, event.to_string()],
        );
    }
    let _ = transaction.commit();
}

fn ephemeral(ty: &str) -> bool {
    matches!(
        ty,
        "session.text.delta"
            | "session.reasoning.delta"
            | "session.tool.input.delta"
            | "session.tool.progress"
            | "session.step.streamed"
            | "session.compaction.delta"
            | "session.message.content.updated"
            | "session.status"
            | "session.idle"
            | "server.notice"
            | "rpc.event"
    )
}

/// Append an event to the durable log. The event is updated in place with
/// `seq`, `created` and a `durable` envelope when it belongs to a session.
pub fn append(event: &mut Value) {
    let ty = event.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    if ephemeral(&ty) {
        return;
    }
    let session = event
        .pointer("/properties/sessionID")
        .or_else(|| event.pointer("/properties/info/id"))
        .and_then(Value::as_str)
        .map(String::from);
    let Some(db) = open() else { return };
    let Ok(mut db) = db.lock() else { return };
    let Ok(transaction) = db.transaction() else { return };
    let seq: i64 = transaction
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |row| row.get(0))
        .unwrap_or(1);
    event["seq"] = json!(seq);
    let created = event
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(crate::util::now_ms);
    event["created"] = json!(created);
    if let Some(session) = &session {
        let aggregate: i64 = transaction
            .query_row(
                "SELECT COUNT(*) + 1 FROM events WHERE session_id = ?1",
                params![session],
                |row| row.get(0),
            )
            .unwrap_or(1);
        event["durable"] = json!({
            "aggregateID": session,
            "seq": aggregate,
            "version": 1,
        });
    }
    let _ = transaction.execute(
        "INSERT INTO events(seq, session_id, type, created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, session.clone().unwrap_or_default(), ty, created, event.to_string()],
    );
    let _ = transaction.commit();
}

/// Read persisted events, optionally scoped to one session and sequence cursor.
pub fn read(session_id: Option<&str>, after: Option<u64>, limit: Option<usize>) -> Vec<Value> {
    let Some(db) = open() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let scoped = session_id.is_some();
    let sql = if scoped {
        // Session logs page by the durable aggregate sequence, which is
        // filtered in Rust because it is stored inside the event envelope.
        "SELECT data FROM events WHERE session_id = ?1 ORDER BY seq"
    } else {
        match (after.is_some(), limit.is_some()) {
            (true, true) => "SELECT data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            (true, false) => "SELECT data FROM events WHERE seq > ?1 ORDER BY seq",
            (false, true) => "SELECT data FROM events ORDER BY seq LIMIT ?1",
            (false, false) => "SELECT data FROM events ORDER BY seq",
        }
    };
    let mut statement = match db.prepare(sql) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(session) = session_id {
        values.push(rusqlite::types::Value::Text(session.to_string()));
    }
    if !scoped {
        if let Some(after) = after {
            values.push(rusqlite::types::Value::Integer(after as i64));
        }
        if let Some(limit) = limit {
            values.push(rusqlite::types::Value::Integer(limit as i64));
        }
    }
    let Ok(mut rows) = statement.query(rusqlite::params_from_iter(values)) else { return Vec::new() };
    let mut out = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(text) = row.get::<_, String>(0) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                out.push(value);
            }
        }
    }
    if scoped {
        if let Some(after) = after {
            out.retain(|value| durable_seq(value) > after);
        }
        if let Some(limit) = limit {
            out.truncate(limit);
        }
    }
    out
}

fn durable_seq(value: &Value) -> u64 {
    value
        .pointer("/durable/seq")
        .or_else(|| value.get("seq"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_events_are_not_written() {
        assert!(ephemeral("session.text.delta"));
        assert!(!ephemeral("session.step.ended"));
    }

    #[test]
    fn durable_events_round_trip_by_session() {
        let session = format!("ses_event_test_{}", crate::util::id("x"));
        let mut event = json!({
            "id": "evt_test",
            "type": "session.updated",
            "properties": { "sessionID": session },
        });
        append(&mut event);
        let items = read(Some(&session), None, Some(10));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["durable"]["aggregateID"], session);
        assert!(read(Some(&session), Some(items[0]["seq"].as_u64().unwrap()), None).is_empty());
    }
}
