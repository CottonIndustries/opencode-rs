//! Tools for per-session schedules.

use super::{req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

fn view(schedule: &crate::schedule::Schedule) -> Value {
    let mut value = serde_json::to_value(schedule).unwrap_or(Value::Null);
    value["prompt"] = schedule.action.get("prompt").cloned().unwrap_or(Value::Null);
    value
}

fn summary(schedule: &crate::schedule::Schedule) -> String {
    let when = schedule.when.get("type").and_then(Value::as_str).unwrap_or("in");
    let trigger = match when {
        "in" => format!("in {}", schedule.when.get("in").and_then(Value::as_str).unwrap_or("")),
        "at" => format!("at {}", schedule.when.get("at").and_then(Value::as_str).unwrap_or("")),
        "every" => format!("every {}", schedule.when.get("every").and_then(Value::as_str).unwrap_or("")),
        "cron" => format!("cron \"{}\"", schedule.when.get("cron").and_then(Value::as_str).unwrap_or("")),
        other => other.to_string(),
    };
    let prompt = schedule.action.get("prompt").and_then(Value::as_str).unwrap_or("");
    let prompt = if prompt.chars().count() > 80 {
        format!("{}...", prompt.chars().take(77).collect::<String>())
    } else {
        prompt.to_string()
    };
    let next = schedule
        .next_due_at
        .map(|next| format!(", next {}", timestamp(next)))
        .unwrap_or_default();
    format!(
        "{}: {} ({}){next}, status {}, prompt: {}",
        schedule.id,
        trigger,
        schedule.tz,
        schedule.status,
        serde_json::to_string(&prompt).unwrap_or_default()
    )
}

fn timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| ms.to_string())
}

fn owned(ctx: &Context, id: &str) -> Result<crate::schedule::Schedule> {
    let schedule = ctx.app.schedules.get(id)?;
    if schedule.session_id != ctx.session_id {
        return Err(anyhow!("Schedule not found: {id}"));
    }
    Ok(schedule)
}

pub struct Create;

#[async_trait]
impl Tool for Create {
    fn name(&self) -> &str {
        "schedule_create"
    }

    fn description(&self) -> String {
        "Schedule a prompt that wakes this session later: once after a delay, at a time, every interval, on a cron, or gated by an `if` / `until` shell check. Returns the schedule with its next run.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "A short label for the schedule." },
                "when": {
                    "type": "object",
                    "description": "When it fires: { in: \"20 minutes\" }, { at: \"2026-10-27T09:00\" }, { every: \"1 hour\" } or { cron: \"0 9 * * 1-5\" }.",
                    "properties": {
                        "in": { "type": "string" },
                        "at": { "type": "string" },
                        "every": { "type": "string" },
                        "cron": { "type": "string" },
                        "tz": { "type": "string" }
                    },
                    "additionalProperties": false
                },
                "if": {
                    "type": "object",
                    "description": "Recurring shell gate: { run } with optional is/matches/changes.",
                    "properties": {
                        "run": { "type": "string" },
                        "is": { "type": "string" },
                        "matches": { "type": "string" },
                        "changes": { "type": "boolean" }
                    },
                    "required": ["run"],
                    "additionalProperties": false
                },
                "until": {
                    "type": "object",
                    "description": "One-shot shell gate: poll until it passes, then wake once.",
                    "properties": {
                        "run": { "type": "string" },
                        "is": { "type": "string" },
                        "matches": { "type": "string" },
                        "changes": { "type": "boolean" }
                    },
                    "required": ["run"],
                    "additionalProperties": false
                },
                "ends": {
                    "type": "object",
                    "properties": {
                        "after": { "type": "string" },
                        "at": { "type": "string" },
                        "times": { "type": "integer", "minimum": 1 }
                    },
                    "additionalProperties": false
                },
                "prompt": { "type": "string", "minLength": 1, "description": "The message the session receives when the schedule wakes." }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        ctx.ask(
            "schedule",
            &["*".into()],
            &["*".into()],
            "Create a schedule".into(),
            input.clone(),
        )
        .await?;
        let directory = ctx.handle.with(|session| session.directory.clone());
        let schedule = ctx.app.schedules.create(&ctx.session_id, &directory, &input).await?;
        let view = view(&schedule);
        Ok(Output::text(format!("Scheduled: {}", summary(&schedule)), "Schedule").meta(view))
    }
}

pub struct List;

#[async_trait]
impl Tool for List {
    fn name(&self) -> &str {
        "schedule_list"
    }

    fn description(&self) -> String {
        "List this session's schedules with their status and next run, and the current time.".into()
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _input: Value, ctx: &Context) -> Result<Output> {
        let schedules = ctx.app.schedules.list(Some(&ctx.session_id));
        let now = timestamp(crate::util::now_ms());
        let content = if schedules.is_empty() {
            format!("Now: {now}\nNo schedules.")
        } else {
            format!(
                "Now: {now}\n{}",
                schedules.iter().map(summary).collect::<Vec<_>>().join("\n")
            )
        };
        Ok(Output::text(content, "Schedules").meta(json!({
            "now": now,
            "schedules": schedules.iter().map(view).collect::<Vec<_>>(),
        })))
    }
}

pub struct Pause;

#[async_trait]
impl Tool for Pause {
    fn name(&self) -> &str {
        "schedule_pause"
    }

    fn description(&self) -> String {
        "Pause a schedule. It keeps its settings and does not fire until resumed.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Schedule id from schedule_list." } },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        owned(ctx, id)?;
        let schedule = ctx.app.schedules.pause(id)?;
        Ok(Output::text(format!("Paused: {}", summary(&schedule)), "Schedule").meta(view(&schedule)))
    }
}

pub struct Resume;

#[async_trait]
impl Tool for Resume {
    fn name(&self) -> &str {
        "schedule_resume"
    }

    fn description(&self) -> String {
        "Resume a paused schedule. A one-shot that fell due while paused fires once now; a repeating one resumes at its next slot.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Schedule id from schedule_list." } },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        owned(ctx, id)?;
        let schedule = ctx.app.schedules.resume(id)?;
        Ok(Output::text(format!("Resumed: {}", summary(&schedule)), "Schedule").meta(view(&schedule)))
    }
}

pub struct Delete;

#[async_trait]
impl Tool for Delete {
    fn name(&self) -> &str {
        "schedule_delete"
    }

    fn description(&self) -> String {
        "Delete a schedule for good.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Schedule id from schedule_list." } },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        owned(ctx, id)?;
        let removed = ctx.app.schedules.remove(id)?;
        Ok(Output::text(format!("Deleted schedule {id}."), "Schedule").meta(json!({ "id": id, "removed": removed })))
    }
}
