//! Append-only durable event log.
//!
//! The log is deliberately small: every plugin-facing event that survives a
//! restart is appended as one JSON object per line. Ephemeral stream deltas are
//! skipped because they are reconstructed from persisted message state.

use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();
static SEQ: OnceLock<AtomicU64> = OnceLock::new();

fn path() -> PathBuf {
    crate::paths::get().storage().join("events.jsonl")
}

fn open() -> Option<&'static Mutex<File>> {
    if let Some(file) = FILE.get() {
        return Some(file);
    }
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new().create(true).append(true).read(true).open(&path).ok()?;
    let _ = FILE.set(Mutex::new(file));
    let next = FILE
        .get()
        .and_then(|file| {
            file.lock()
                .ok()
                .and_then(|file| BufReader::new(&*file).lines().map_while(Result::ok).last())
                .and_then(|line| serde_json::from_str::<Value>(&line).ok())
                .and_then(|value| value.get("seq").and_then(Value::as_u64))
                .map(|seq| seq + 1)
        })
        .unwrap_or(1);
    let _ = SEQ.set(AtomicU64::new(next));
    FILE.get()
}

fn next_seq() -> u64 {
    let _ = open();
    SEQ.get().map(|seq| seq.fetch_add(1, Ordering::SeqCst)).unwrap_or(1)
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
/// `seq` and a `durable` envelope when it belongs to a session.
pub fn append(event: &mut Value) {
    let ty = event.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    if ephemeral(&ty) {
        return;
    }
    let session = event
        .pointer("/properties/sessionID")
        .and_then(Value::as_str)
        .map(String::from);
    let seq = next_seq();
    event["seq"] = json!(seq);
    event["created"] = event
        .get("created")
        .cloned()
        .unwrap_or_else(|| json!(crate::util::now_ms()));
    if let Some(session) = session {
        event["durable"] = json!({
            "aggregateID": session,
            "seq": seq,
            "version": 1,
        });
    }
    let Ok(line) = serde_json::to_string(event) else { return };
    if let Some(file) = open() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// Read persisted events, optionally scoped to one session and sequence cursor.
pub fn read(session_id: Option<&str>, after: Option<u64>, limit: Option<usize>) -> Vec<Value> {
    let Some(file) = open() else { return Vec::new() };
    let Ok(file) = file.lock() else { return Vec::new() };
    let reader = BufReader::new(&*file);
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(after) = after {
            if value.get("seq").and_then(Value::as_u64).unwrap_or(0) <= after {
                continue;
            }
        }
        if let Some(session_id) = session_id {
            let matches = value
                .pointer("/properties/sessionID")
                .and_then(Value::as_str)
                .is_some_and(|id| id == session_id);
            if !matches {
                continue;
            }
        }
        out.push(value);
        if limit.is_some_and(|limit| out.len() >= limit) {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_events_are_not_written() {
        assert!(ephemeral("session.text.delta"));
        assert!(!ephemeral("session.step.ended"));
    }
}
