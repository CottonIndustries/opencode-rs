//! Durable background job registry.
//!
//! Jobs are process-local while running and persisted to SQLite so the HTTP
//! and plugin layers can list them after a restart. A backgrounded job keeps
//! a notification id; when it settles the owning session receives a synthetic
//! completion message.

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub session_id: String,
    pub title: String,
    pub status: String,
    pub started: i64,
    pub completed: Option<i64>,
    pub details: Value,
    pub notification_id: Option<String>,
    pub backgrounded: bool,
    pub consumed: bool,
    pub output: Option<String>,
    pub error: Option<String>,
    notify: Arc<Notify>,
}

static APP: OnceLock<Mutex<Weak<crate::app::App>>> = OnceLock::new();

fn jobs() -> &'static Mutex<BTreeMap<String, Job>> {
    static JOBS: OnceLock<Mutex<BTreeMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn db_path() -> PathBuf {
    crate::paths::get().storage().join("jobs.db")
}

fn db() -> Option<&'static Mutex<Connection>> {
    static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
    if let Some(db) = DB.get() {
        return Some(db);
    }
    let path = db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let connection = Connection::open(&path).ok()?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                session_id TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                started INTEGER NOT NULL,
                completed INTEGER,
                details TEXT NOT NULL,
                notification_id TEXT,
                backgrounded INTEGER NOT NULL DEFAULT 0,
                consumed INTEGER NOT NULL DEFAULT 0,
                output TEXT,
                error TEXT
             );
             CREATE INDEX IF NOT EXISTS jobs_session ON jobs(session_id, started);
             CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status, started);",
        )
        .ok()?;
    let now = crate::util::now_ms();
    let _ = connection.execute(
        "UPDATE jobs SET status = 'error', error = 'interrupted by restart', completed = ?1
          WHERE status = 'running'",
        params![now],
    );
    hydrate(&connection);
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

fn hydrate(connection: &Connection) {
    let Ok(mut statement) = connection.prepare("SELECT * FROM jobs") else { return };
    let Ok(mut rows) = statement.query([]) else { return };
    let mut jobs = jobs().lock().unwrap();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(job) = row_to_job(row) {
            jobs.entry(job.id.clone()).or_insert(job);
        }
    }
}

/// Attach the application so completed background jobs can notify their session.
pub fn attach(app: &Arc<crate::app::App>) {
    let holder = APP.get_or_init(|| Mutex::new(Weak::new()));
    if let Ok(mut holder) = holder.lock() {
        *holder = Arc::downgrade(app);
    }
}

fn persist(job: &Job) {
    let Some(db) = db() else { return };
    let Ok(db) = db.lock() else { return };
    let _ = db.execute(
        "INSERT INTO jobs(id, kind, session_id, title, status, started, completed, details, notification_id, backgrounded, consumed, output, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
            kind = excluded.kind, session_id = excluded.session_id, title = excluded.title,
            status = excluded.status, started = excluded.started, completed = excluded.completed,
            details = excluded.details, notification_id = excluded.notification_id,
            backgrounded = excluded.backgrounded, consumed = excluded.consumed,
            output = excluded.output, error = excluded.error",
        params![
            job.id,
            job.kind,
            job.session_id,
            job.title,
            job.status,
            job.started,
            job.completed,
            job.details.to_string(),
            job.notification_id,
            job.backgrounded as i64,
            job.consumed as i64,
            job.output,
            job.error,
        ],
    );
}

pub fn create(kind: &str, session_id: &str, title: &str, details: Value) -> String {
    let id = crate::util::id("job");
    let job = Job {
        id: id.clone(),
        kind: kind.to_string(),
        session_id: session_id.to_string(),
        title: title.to_string(),
        status: "running".into(),
        started: crate::util::now_ms(),
        completed: None,
        details,
        notification_id: None,
        backgrounded: false,
        consumed: false,
        output: None,
        error: None,
        notify: Arc::new(Notify::new()),
    };
    persist(&job);
    jobs().lock().unwrap().insert(id.clone(), job);
    id
}

/// Finish a job. The status and any extra details are persisted.
pub fn finish(id: &str, status: &str, details: Option<Value>) {
    let (job, background) = {
        let mut jobs = jobs().lock().unwrap();
        let Some(job) = jobs.get_mut(id) else { return };
        job.status = status.to_string();
        job.completed = Some(crate::util::now_ms());
        if let (Some(target), Some(Value::Object(extra))) = (job.details.as_object_mut(), details) {
            for (key, value) in extra {
                target.insert(key, value);
            }
        }
        if let Some(error) = job.details.get("error").and_then(Value::as_str) {
            job.error = Some(error.to_string());
        }
        if let Some(output) = job.details.get("output").and_then(Value::as_str) {
            job.output = Some(output.to_string());
        }
        (job.clone(), job.backgrounded && job.notification_id.is_some() && status != "cancelled")
    };
    persist(&job);
    job.notify.notify_one();
    if background {
        notify_completion(job);
    }
}

/// Resume completion notifications for background jobs that settled before a restart.
pub fn resume(app: Arc<crate::app::App>) {
    if std::env::var_os("OPENCODE_RESUME_BACKGROUND").is_none() {
        return;
    }
    let _ = db();
    for job in pending_background() {
        let Some(notification) = job.get("notificationID").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            let notification_id = notification.clone();
            crate::engine::notify_with_id(app, job["sessionID"].as_str().unwrap_or_default().to_string(), text_for(&job), Some(notification_id.clone()), Some(json!({ "background": true, "jobID": job["id"] }))).await;
            complete_background(&notification_id);
        });
    }
}

fn text_for(job: &Value) -> String {
    format!(
        "Background job {} ({}) finished with status {}.",
        job.get("id").and_then(Value::as_str).unwrap_or("job"),
        job.get("kind").and_then(Value::as_str).unwrap_or("job"),
        job.get("status").and_then(Value::as_str).unwrap_or("unknown"),
    )
}

fn notify_completion(job: Job) {
    let Some(holder) = APP.get() else { return };
    let Ok(holder) = holder.lock() else { return };
    let Some(app) = holder.upgrade() else { return };
    let Some(notification) = job.notification_id.clone() else { return };
    let text = format!(
        "Background job {} ({}) finished with status {}.",
        job.id, job.kind, job.status
    );
    tokio::spawn(async move {
        let metadata = json!({ "background": true, "jobID": job.id, "kind": job.kind });
        crate::engine::notify_with_id(app, job.session_id, text, Some(notification), Some(metadata)).await;
    });
}

pub fn cancel(id: &str) -> bool {
    let job = {
        let mut jobs = jobs().lock().unwrap();
        let Some(job) = jobs.get_mut(id) else { return false };
        job.status = "cancelled".into();
        job.completed.get_or_insert_with(crate::util::now_ms);
        job.clone()
    };
    persist(&job);
    job.notify.notify_one();
    true
}

pub fn get(id: &str) -> Option<Value> {
    if let Some(job) = jobs().lock().unwrap().get(id).cloned() {
        return Some(encode(&job));
    }
    let db = db()?;
    let db = db.lock().ok()?;
    let mut statement = db.prepare("SELECT * FROM jobs WHERE id = ?1").ok()?;
    let mut rows = statement.query(params![id]).ok()?;
    let row = rows.next().ok()??;
    row_to_job(row).ok().map(|job| encode(&job))
}

pub fn list(session: Option<&str>, kind: Option<&str>, status: Option<&str>, descendants: bool) -> Vec<Value> {
    let mut jobs = jobs().lock().unwrap().values().cloned().collect::<Vec<_>>();
    if let Some(db) = db() {
        if let Ok(db) = db.lock() {
            if let Ok(mut statement) = db.prepare("SELECT * FROM jobs ORDER BY started, id") {
                if let Ok(mut rows) = statement.query([]) {
                    while let Ok(Some(row)) = rows.next() {
                        if let Ok(job) = row_to_job(row) {
                            if !jobs.iter().any(|existing| existing.id == job.id) {
                                jobs.push(job);
                            }
                        }
                    }
                }
            }
        }
    }
    jobs.into_iter()
        .filter(|job| {
            if let Some(session) = session {
                let matches = if descendants {
                    job.session_id == session || is_descendant(&job.session_id, session)
                } else {
                    job.session_id == session
                };
                if !matches {
                    return false;
                }
            }
            kind.is_none_or(|kind| job.kind == kind) && status.is_none_or(|status| job.status == status)
        })
        .map(|job| encode(&job))
        .collect()
}

fn is_descendant(from: &str, ancestor: &str) -> bool {
    let mut current = from.to_string();
    let mut seen = std::collections::BTreeSet::new();
    while seen.insert(current.clone()) {
        if current == ancestor {
            return true;
        }
        let Some(parent) = parent_of(&current) else { return false };
        current = parent;
    }
    false
}

fn parent_of(session: &str) -> Option<String> {
    let text = crate::db::load(session)?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("parent_id")
        .or_else(|| value.get("parentID"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Mark one running job as backgrounded and return its info.
pub fn background(id: &str) -> Option<Value> {
    let mut jobs = jobs().lock().unwrap();
    let job = jobs.get_mut(id)?;
    if job.status != "running" && !job.backgrounded {
        return Some(encode(job));
    }
    if !job.backgrounded {
        job.backgrounded = true;
        job.notification_id = Some(crate::util::id("msg"));
    }
    let job = job.clone();
    drop(jobs);
    persist(&job);
    Some(encode(&job))
}

/// Background all blocking jobs belonging to a session or its descendants.
pub fn background_all(session: &str) -> Vec<Value> {
    let mut jobs = jobs().lock().unwrap();
    let mut out = Vec::new();
    for job in jobs.values_mut() {
        if job.status != "running" || job.backgrounded {
            continue;
        }
        if job.session_id != session && !is_descendant(&job.session_id, session) {
            continue;
        }
        job.backgrounded = true;
        job.notification_id = Some(crate::util::id("msg"));
        out.push(job.clone());
    }
    drop(jobs);
    for job in &out {
        persist(job);
    }
    out.iter().map(encode).collect()
}

pub fn cancel_session(session: &str) -> Vec<Value> {
    let mut jobs = jobs().lock().unwrap();
    let mut out = Vec::new();
    for job in jobs.values_mut() {
        if job.status != "running" || (job.session_id != session && !is_descendant(&job.session_id, session)) {
            continue;
        }
        job.status = "cancelled".into();
        job.completed = Some(crate::util::now_ms());
        out.push(job.clone());
    }
    drop(jobs);
    for job in &out {
        persist(job);
        job.notify.notify_one();
    }
    out.iter().map(encode).collect()
}

pub fn pending_background() -> Vec<Value> {
    jobs()
        .lock()
        .unwrap()
        .values()
        .filter(|job| job.backgrounded && !job.consumed && job.status != "cancelled")
        .map(encode)
        .collect()
}

pub fn complete_background(notification_id: &str) {
    let mut jobs = jobs().lock().unwrap();
    for job in jobs.values_mut() {
        if job.notification_id.as_deref() == Some(notification_id) {
            job.consumed = true;
            let job = job.clone();
            drop(jobs);
            persist(&job);
            return;
        }
    }
}

/// Wait for a job to settle. Returns the latest info and whether it timed out.
pub async fn wait(id: &str, timeout: Option<Duration>) -> (Option<Value>, bool) {
    let Some((job, notify)) = jobs()
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .map(|job| (job.clone(), job.notify.clone()))
    else {
        return (None, false);
    };
    if job.status != "running" {
        return (Some(encode(&job)), false);
    }
    let notified = notify.notified();
    if let Some(timeout) = timeout {
        if tokio::time::timeout(timeout, notified).await.is_err() {
            return (get(id), true);
        }
    } else {
        notified.await;
    }
    (get(id), false)
}

fn encode(job: &Job) -> Value {
    let mut value = json!({
        "id": job.id,
        "kind": job.kind,
        "type": job.kind,
        "sessionID": job.session_id,
        "status": job.status,
        "title": job.title,
        "backgrounded": job.backgrounded,
        "time": {
            "started": job.started,
            "completed": job.completed,
        },
    });
    if let Some(notification) = &job.notification_id {
        value["notificationID"] = json!(notification);
    }
    if let Some(output) = &job.output {
        value["output"] = json!(output);
    }
    if let Some(error) = &job.error {
        value["error"] = json!(error);
    }
    if let (Some(target), Value::Object(extra)) = (value.as_object_mut(), job.details.clone()) {
        for (key, item) in extra {
            target.insert(key, item);
        }
    }
    value
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let details: String = row.get("details")?;
    Ok(Job {
        id: row.get("id")?,
        kind: row.get("kind")?,
        session_id: row.get("session_id")?,
        title: row.get("title")?,
        status: row.get("status")?,
        started: row.get("started")?,
        completed: row.get("completed")?,
        details: serde_json::from_str(&details).unwrap_or(Value::Null),
        notification_id: row.get("notification_id")?,
        backgrounded: row.get::<_, i64>("backgrounded")? != 0,
        consumed: row.get::<_, i64>("consumed")? != 0,
        output: row.get("output")?,
        error: row.get("error")?,
        notify: Arc::new(Notify::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_track_lifecycle_and_filters() {
        let session = format!("ses_job_test_{}", crate::util::id("x"));
        let id = create("shell", &session, "echo hi", json!({ "shellID": "sh_1", "command": "echo hi" }));
        assert_eq!(list(Some(&session), Some("shell"), Some("running"), false).len(), 1);
        finish(&id, "completed", Some(json!({ "exit": 0 })));
        let items = list(Some(&session), None, Some("completed"), false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["exit"], 0);
        assert!(cancel(&id));
        assert_eq!(get(&id).unwrap()["status"], "cancelled");
    }

    #[tokio::test]
    async fn background_jobs_wait_and_complete() {
        let session = format!("ses_job_background_{}", crate::util::id("x"));
        let id = create("shell", &session, "sleep", json!({ "command": "sleep" }));
        let background = background(&id).unwrap();
        assert_eq!(background["backgrounded"], true);
        assert!(background["notificationID"].as_str().is_some());
        let wait_id = id.clone();
        let waiter = tokio::spawn(async move { wait(&wait_id, Some(Duration::from_secs(2))).await });
        finish(&id, "completed", Some(json!({ "exit": 0 })));
        let (info, timed_out) = waiter.await.unwrap();
        assert!(!timed_out);
        assert_eq!(info.unwrap()["status"], "completed");
        let notification = background["notificationID"].as_str().unwrap().to_string();
        complete_background(&notification);
        assert!(pending_background().iter().all(|job| job["id"] != id));
    }
}
