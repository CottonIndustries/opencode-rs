//! Append-only durable event log.
//!
//! Events live in SQLite per aggregate (normally a session) with a durable
//! sequence independent of the global insertion order. The legacy JSONL log
//! is imported once so existing installations keep replayable history.

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

fn column(connection: &Connection, table: &str, name: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut statement) = connection.prepare(&sql) else { return false };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else { return false };
    let names: Vec<String> = rows.flatten().collect();
    names.iter().any(|value| value == name)
}

fn ensure_column(connection: &Connection, table: &str, name: &str, ddl: &str) {
    if !column(connection, table, name) {
        let _ = connection.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ddl}"), []);
    }
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
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY,
                id TEXT,
                aggregate_id TEXT,
                aggregate_seq INTEGER,
                session_id TEXT,
                type TEXT NOT NULL,
                created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
             CREATE TABLE IF NOT EXISTS event_sequence (
                aggregate_id TEXT PRIMARY KEY,
                seq INTEGER NOT NULL
            );",
        )
        .ok()?;
    ensure_column(&connection, "events", "id", "TEXT");
    ensure_column(&connection, "events", "aggregate_id", "TEXT");
    ensure_column(&connection, "events", "aggregate_seq", "INTEGER");
    backfill(&connection);
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS events_session ON events(session_id, seq);
             CREATE INDEX IF NOT EXISTS events_type ON events(type, seq);
             CREATE UNIQUE INDEX IF NOT EXISTS events_aggregate_seq ON events(aggregate_id, aggregate_seq)
                WHERE aggregate_id IS NOT NULL AND aggregate_seq IS NOT NULL;",
        )
        .ok()?;
    migrate(&mut connection);
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

/// Fill aggregate columns for databases created before per-aggregate sequences.
fn backfill(connection: &Connection) {
    let _ = connection.execute(
        "UPDATE events
            SET aggregate_id = session_id
          WHERE aggregate_id IS NULL AND session_id IS NOT NULL AND session_id != ''",
        [],
    );
    let _ = connection.execute("UPDATE events SET id = 'evt_' || seq WHERE id IS NULL OR id = ''", []);
    let _ = connection.execute(
        "WITH ranked AS (
            SELECT seq, ROW_NUMBER() OVER (PARTITION BY aggregate_id ORDER BY seq) AS rn
              FROM events
             WHERE aggregate_id IS NOT NULL AND aggregate_id != ''
         )
         UPDATE events
            SET aggregate_seq = (SELECT rn FROM ranked WHERE ranked.seq = events.seq)
          WHERE aggregate_id IS NOT NULL AND aggregate_id != '' AND aggregate_seq IS NULL",
        [],
    );
    let _ = connection.execute(
        "INSERT INTO event_sequence(aggregate_id, seq)
         SELECT aggregate_id, MAX(aggregate_seq) FROM events
          WHERE aggregate_id IS NOT NULL AND aggregate_id != ''
          GROUP BY aggregate_id
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = MAX(seq, excluded.seq)",
        [],
    );
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
        let id = event.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("evt_{seq}"));
        let aggregate_seq: i64 = transaction
            .query_row(
                "SELECT COALESCE((SELECT seq FROM event_sequence WHERE aggregate_id = ?1), 0) + 1",
                params![session],
                |row| row.get(0),
            )
            .unwrap_or(1);
        let _ = transaction.execute(
            "INSERT OR IGNORE INTO events(seq, id, aggregate_id, aggregate_seq, session_id, type, created, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![seq, id, session, aggregate_seq, session, ty, created, event.to_string()],
        );
        let _ = transaction.execute(
            "INSERT INTO event_sequence(aggregate_id, seq) VALUES (?1, ?2)
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = MAX(seq, excluded.seq)",
            params![session, aggregate_seq],
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
/// id, global seq, created, and a durable envelope when it belongs to an
/// aggregate.
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
    let id = event
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
        .unwrap_or_else(|| crate::util::id("evt"));
    let created = event
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| crate::util::now_ms());
    event["id"] = json!(id);
    event["seq"] = json!(seq);
    event["created"] = json!(created);
    let aggregate_seq = if let Some(session) = &session {
        let next: i64 = transaction
            .query_row(
                "SELECT COALESCE((SELECT seq FROM event_sequence WHERE aggregate_id = ?1), 0) + 1",
                params![session],
                |row| row.get(0),
            )
            .unwrap_or(1);
        let _ = transaction.execute(
            "INSERT INTO event_sequence(aggregate_id, seq) VALUES (?1, ?2)
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
            params![session, next],
        );
        event["durable"] = json!({
            "aggregateID": session,
            "seq": next,
            "version": 1,
        });
        Some(next)
    } else {
        None
    };
    let _ = transaction.execute(
        "INSERT INTO events(seq, id, aggregate_id, aggregate_seq, session_id, type, created, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            seq,
            event["id"].as_str().unwrap_or_default(),
            session.clone(),
            aggregate_seq,
            session.unwrap_or_default(),
            ty,
            created,
            event.to_string(),
        ],
    );
    let _ = transaction.commit();
}

pub fn contains(text: &str) -> bool {
    let Some(db) = open() else { return false };
    let Ok(db) = db.lock() else { return false };
    db.query_row(
        "SELECT 1 FROM events WHERE data LIKE '%' || ?1 || '%' LIMIT 1",
        params![text],
        |_| Ok(()),
    )
    .is_ok()
}

/// Read persisted events. Session scoped reads page by the aggregate sequence.
pub fn read(session_id: Option<&str>, after: Option<u64>, limit: Option<usize>) -> Vec<Value> {
    let Some(db) = open() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(session) = session_id {
        let sql = match limit {
            Some(_) => "SELECT data FROM events WHERE aggregate_id = ?1 AND aggregate_seq > ?2 ORDER BY aggregate_seq LIMIT ?3",
            None => "SELECT data FROM events WHERE aggregate_id = ?1 AND aggregate_seq > ?2 ORDER BY aggregate_seq",
        };
        let mut values: Vec<rusqlite::types::Value> = vec![
            rusqlite::types::Value::Text(session.to_string()),
            rusqlite::types::Value::Integer(after.unwrap_or(0) as i64),
        ];
        if let Some(limit) = limit {
            values.push(rusqlite::types::Value::Integer(limit as i64));
        }
        let Ok(mut statement) = db.prepare(sql) else { return out };
        let Ok(mut rows) = statement.query(rusqlite::params_from_iter(values)) else { return out };
        while let Ok(Some(row)) = rows.next() {
            if let Ok(text) = row.get::<_, String>(0) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    out.push(value);
                }
            }
        }
        return out;
    }
    let sql = match (after.is_some(), limit.is_some()) {
        (true, true) => "SELECT data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        (true, false) => "SELECT data FROM events WHERE seq > ?1 ORDER BY seq",
        (false, true) => "SELECT data FROM events ORDER BY seq LIMIT ?1",
        (false, false) => "SELECT data FROM events ORDER BY seq",
    };
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(after) = after {
        values.push(rusqlite::types::Value::Integer(after as i64));
    }
    if let Some(limit) = limit {
        values.push(rusqlite::types::Value::Integer(limit as i64));
    }
    let Ok(mut statement) = db.prepare(sql) else { return out };
    let Ok(mut rows) = statement.query(rusqlite::params_from_iter(values)) else { return out };
    while let Ok(Some(row)) = rows.next() {
        if let Ok(text) = row.get::<_, String>(0) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                out.push(value);
            }
        }
    }
    out
}

/// Current durable sequence for an aggregate, or zero when it has no events.
pub fn sequence(aggregate: &str) -> u64 {
    let Some(db) = open() else { return 0 };
    let Ok(db) = db.lock() else { return 0 };
    db.query_row(
        "SELECT COALESCE((SELECT seq FROM event_sequence WHERE aggregate_id = ?1), 0)",
        params![aggregate],
        |row| row.get::<_, i64>(0),
    )
    .map(|seq| seq as u64)
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
    fn durable_events_round_trip_by_aggregate() {
        let session = format!("ses_event_test_{}", crate::util::id("x"));
        for index in 0..2 {
            let mut event = json!({
                "id": format!("evt_test_{index}"),
                "type": "session.updated",
                "properties": { "sessionID": session },
            });
            append(&mut event);
        }
        let other = format!("ses_event_other_{}", crate::util::id("x"));
        let mut event = json!({ "id": "evt_other", "type": "session.updated", "properties": { "sessionID": other } });
        append(&mut event);
        assert_eq!(sequence(&session), 2);
        assert_eq!(sequence(&other), 1);
        let items = read(Some(&session), None, Some(10));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["durable"]["seq"], 1);
        assert_eq!(items[1]["durable"]["seq"], 2);
        assert_eq!(read(Some(&session), Some(1), None).len(), 1);
        assert!(read(Some(&session), Some(2), None).is_empty());
    }
}
