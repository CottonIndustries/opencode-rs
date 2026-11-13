//! Background jobs spawned by tools and media generation.
//!
//! Jobs are process-local and intentionally small: the HTTP and plugin layers
//! use them to list work that is still running, including shell commands,
//! background subagents and image generation.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub session_id: String,
    pub title: String,
    pub status: String,
    pub started: i64,
    pub completed: Option<i64>,
    pub details: Value,
}

fn jobs() -> &'static Mutex<BTreeMap<String, Job>> {
    static JOBS: OnceLock<Mutex<BTreeMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(BTreeMap::new()))
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
    };
    jobs().lock().unwrap().insert(id.clone(), job);
    id
}

/// Finish a job. `details` is merged into the job's detail object.
pub fn finish(id: &str, status: &str, details: Option<Value>) {
    let mut jobs = jobs().lock().unwrap();
    if let Some(job) = jobs.get_mut(id) {
        job.status = status.to_string();
        job.completed = Some(crate::util::now_ms());
        if let (Some(target), Some(Value::Object(extra))) = (job.details.as_object_mut(), details) {
            for (key, value) in extra {
                target.insert(key, value);
            }
        }
    }
}

pub fn cancel(id: &str) -> bool {
    let mut jobs = jobs().lock().unwrap();
    let Some(job) = jobs.get_mut(id) else { return false };
    job.status = "cancelled".into();
    job.completed.get_or_insert_with(crate::util::now_ms);
    true
}

pub fn get(id: &str) -> Option<Value> {
    jobs().lock().unwrap().get(id).map(encode)
}

pub fn list(session: Option<&str>, kind: Option<&str>, status: Option<&str>, descendants: bool) -> Vec<Value> {
    let jobs = jobs().lock().unwrap();
    jobs.values()
        .filter(|job| {
            if let Some(session) = session {
                if descendants {
                    if job.session_id != session && !job.session_id.starts_with(&format!("{session}:")) {
                        return false;
                    }
                } else if job.session_id != session {
                    return false;
                }
            }
            kind.is_none_or(|kind| job.kind == kind)
                && status.is_none_or(|status| job.status == status)
        })
        .map(encode)
        .collect()
}

fn encode(job: &Job) -> Value {
    let mut value = json!({
        "id": job.id,
        "kind": job.kind,
        "sessionID": job.session_id,
        "status": job.status,
        "title": job.title,
        "time": {
            "started": job.started,
            "completed": job.completed,
        },
    });
    if let (Some(target), Value::Object(extra)) = (value.as_object_mut(), job.details.clone()) {
        for (key, item) in extra {
            target.insert(key, item);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_track_lifecycle_and_filters() {
        let id = create("shell", "ses_job_test", "echo hi", json!({ "shellID": "sh_1", "command": "echo hi" }));
        assert_eq!(list(Some("ses_job_test"), Some("shell"), Some("running"), false).len(), 1);
        finish(&id, "completed", Some(json!({ "exit": 0 })));
        let items = list(Some("ses_job_test"), None, Some("completed"), false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["exit"], 0);
        assert!(cancel(&id));
        assert_eq!(list(None, None, Some("cancelled"), false).len(), 1);
    }
}
