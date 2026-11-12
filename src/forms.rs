//! Session forms: structured questions a plugin or client asks the user, and
//! the replies or cancellation that settle them.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// What happened to a form a waiter was waiting on.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Replied(Value),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Answered,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Form {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    pub fields: Value,
    pub status: Status,
    pub replies: Value,
    pub time_created: i64,
    pub time_updated: i64,
}

struct Entry {
    form: Form,
    waiters: Vec<oneshot::Sender<Outcome>>,
}

#[derive(Default)]
pub struct FormStore {
    inner: Mutex<HashMap<String, Entry>>,
}

impl FormStore {
    /// Create a pending form owned by a session.
    pub fn create(&self, session_id: &str, title: &str, metadata: Value, fields: Value) -> Form {
        let now = crate::util::now_ms();
        let form = Form {
            id: crate::util::id("frm"),
            session_id: session_id.to_string(),
            title: title.to_string(),
            metadata,
            fields,
            status: Status::Pending,
            replies: json!({}),
            time_created: now,
            time_updated: now,
        };
        self.inner.lock().unwrap().insert(form.id.clone(), Entry { form: form.clone(), waiters: Vec::new() });
        form
    }

    /// Pending forms for a session, oldest first.
    pub fn list(&self, session_id: &str) -> Vec<Form> {
        let inner = self.inner.lock().unwrap();
        let mut forms: Vec<Form> = inner
            .values()
            .filter(|entry| entry.form.session_id == session_id && entry.form.status == Status::Pending)
            .map(|entry| entry.form.clone())
            .collect();
        forms.sort_by(|a, b| a.time_created.cmp(&b.time_created).then_with(|| a.id.cmp(&b.id)));
        forms
    }

    pub fn get(&self, id: &str) -> Result<Form> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|entry| entry.form.clone())
            .ok_or_else(|| anyhow!("form not found: {id}"))
    }

    /// Record a reply and wake waiters. A settled form cannot be answered again.
    pub fn reply(&self, id: &str, values: Value) -> Result<Form> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.get_mut(id).ok_or_else(|| anyhow!("form not found: {id}"))?;
        match entry.form.status {
            Status::Pending => {}
            Status::Answered => return Err(anyhow!("form already answered: {id}")),
            Status::Cancelled => return Err(anyhow!("form already cancelled: {id}")),
        }
        match (&mut entry.form.replies, values) {
            (Value::Object(current), Value::Object(next)) => current.extend(next),
            (current, next) => *current = next,
        }
        entry.form.status = Status::Answered;
        entry.form.time_updated = crate::util::now_ms();
        let form = entry.form.clone();
        let waiters = std::mem::take(&mut entry.waiters);
        drop(inner);
        for waiter in waiters {
            let _ = waiter.send(Outcome::Replied(form.replies.clone()));
        }
        Ok(form)
    }

    /// Mark a form cancelled and wake waiters.
    pub fn cancel(&self, id: &str) -> Result<Form> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.get_mut(id).ok_or_else(|| anyhow!("form not found: {id}"))?;
        match entry.form.status {
            Status::Pending => {}
            Status::Answered => return Err(anyhow!("form already answered: {id}")),
            Status::Cancelled => return Err(anyhow!("form already cancelled: {id}")),
        }
        entry.form.status = Status::Cancelled;
        entry.form.time_updated = crate::util::now_ms();
        let form = entry.form.clone();
        let waiters = std::mem::take(&mut entry.waiters);
        drop(inner);
        for waiter in waiters {
            let _ = waiter.send(Outcome::Cancelled);
        }
        Ok(form)
    }

    /// Wait for the form to be answered or cancelled.
    pub async fn wait(&self, id: &str) -> Result<Outcome> {
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner.get_mut(id).ok_or_else(|| anyhow!("form not found: {id}"))?;
            match entry.form.status {
                Status::Answered => return Ok(Outcome::Replied(entry.form.replies.clone())),
                Status::Cancelled => return Ok(Outcome::Cancelled),
                Status::Pending => {}
            }
            let (tx, rx) = oneshot::channel();
            entry.waiters.push(tx);
            rx
        };
        rx.await.map_err(|_| anyhow!("form closed: {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create(store: &FormStore) -> Form {
        store.create(
            "ses_1",
            "Connect account",
            json!({ "provider": "example" }),
            json!([{ "key": "token", "title": "Token", "type": "string", "required": true }]),
        )
    }

    #[test]
    fn create_get_and_list() {
        let store = FormStore::default();
        let form = create(&store);
        assert!(form.id.starts_with("frm_"));
        assert_eq!(form.status, Status::Pending);
        assert_eq!(store.get(&form.id).unwrap().title, "Connect account");
        assert_eq!(store.list("ses_1").len(), 1);
        assert!(store.list("ses_2").is_empty());
        assert!(store.get("frm_missing").is_err());
    }

    #[test]
    fn forms_serialize_with_camel_case_names() {
        let store = FormStore::default();
        let value = serde_json::to_value(create(&store)).unwrap();
        assert_eq!(value["sessionID"], "ses_1");
        assert_eq!(value["status"], "pending");
        assert!(value.get("timeCreated").is_some());
        assert!(value.get("timeUpdated").is_some());
        assert_eq!(value["replies"], json!({}));
    }

    #[tokio::test]
    async fn reply_settles_and_wakes_waiters() {
        let store = std::sync::Arc::new(FormStore::default());
        let form = create(&store);
        let waiting = {
            let store = store.clone();
            let id = form.id.clone();
            tokio::spawn(async move { store.wait(&id).await })
        };
        store.reply(&form.id, json!({ "token": "abc" })).unwrap();
        let outcome = waiting.await.unwrap().unwrap();
        assert_eq!(outcome, Outcome::Replied(json!({ "token": "abc" })));
        assert_eq!(store.get(&form.id).unwrap().status, Status::Answered);
        assert!(store.list("ses_1").is_empty());
        assert!(store.reply(&form.id, json!({ "token": "again" })).is_err());
    }

    #[tokio::test]
    async fn cancel_settles_and_wakes_waiters() {
        let store = std::sync::Arc::new(FormStore::default());
        let form = create(&store);
        let waiting = {
            let store = store.clone();
            let id = form.id.clone();
            tokio::spawn(async move { store.wait(&id).await })
        };
        store.cancel(&form.id).unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), Outcome::Cancelled);
        assert_eq!(store.get(&form.id).unwrap().status, Status::Cancelled);
        assert!(store.list("ses_1").is_empty());
        assert!(store.cancel(&form.id).is_err());
    }

    #[tokio::test]
    async fn wait_returns_immediately_for_settled_forms() {
        let store = FormStore::default();
        let form = create(&store);
        store.reply(&form.id, json!({ "token": "abc" })).unwrap();
        assert_eq!(store.wait(&form.id).await.unwrap(), Outcome::Replied(json!({ "token": "abc" })));
        assert!(store.wait("frm_missing").await.is_err());
    }

    #[test]
    fn replies_are_merged() {
        let store = FormStore::default();
        let form = create(&store);
        store.reply(&form.id, json!({ "first": 1, "second": 2 })).unwrap();
        assert_eq!(store.get(&form.id).unwrap().replies, json!({ "first": 1, "second": 2 }));
    }
}
