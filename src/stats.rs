//! Read-only session statistics shared by the CLI (`opencode stats`) and the TUI.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Aggregate stored sessions. `days` keeps only activity from the last N days.
pub fn data(days: Option<i64>) -> anyhow::Result<Value> {
    let root = crate::paths::get().storage().join("session");
    let since = days.map(|d| {
        let start = chrono::Local::now().date_naive() - chrono::Duration::days(d);
        start.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::Local).unwrap().timestamp_millis()
    });
    let mut sessions = 0u64;
    let mut messages = 0u64;
    let mut usage = crate::session::Usage::default();
    let mut cost = 0.0f64;
    let mut tools: BTreeMap<String, (u64, u64)> = Default::default();
    let mut models: BTreeMap<String, (u64, f64)> = Default::default();
    for proj in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let Ok(bytes) = std::fs::read(f.path()) else { continue };
            let Ok(s) = serde_json::from_slice::<crate::session::Session>(&bytes) else { continue };
            if since.is_some_and(|t| s.time_updated < t) {
                continue;
            }
            sessions += 1;
            for m in &s.messages {
                if since.is_some_and(|t| m.time < t) {
                    continue;
                }
                messages += 1;
                usage.add(&m.usage);
                cost += m.cost;
                if let Some(model) = &m.model {
                    if m.role == crate::session::Role::Assistant {
                        let e = models.entry(model.clone()).or_default();
                        e.0 += 1;
                        e.1 += m.cost;
                    }
                }
                for p in &m.parts {
                    if let crate::session::Part::Tool { name, state, .. } = p {
                        let e = tools.entry(name.clone()).or_default();
                        e.0 += 1;
                        if matches!(state, crate::session::ToolState::Error { .. }) {
                            e.1 += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(json!({
        "sessions": sessions,
        "messages": messages,
        "usage": usage,
        "cost": cost,
        "models": models,
        "tools": tools,
    }))
}

fn count(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn amount(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

/// Plain-text report for terminals and the TUI's scrollable text dialog.
pub fn format(data: &Value) -> String {
    let usage = data.get("usage").unwrap_or(&Value::Null);
    let cost = amount(data.get("cost"));
    let mut out = String::new();
    let _ = writeln!(out, "Sessions      {}", count(data.get("sessions")));
    let _ = writeln!(out, "Messages      {}", count(data.get("messages")));
    let _ = writeln!(out, "Input tokens  {}", count(usage.get("input")));
    let _ = writeln!(out, "Output tokens {}", count(usage.get("output")));
    let _ = writeln!(out, "Cache read    {}", count(usage.get("cache_read")));
    let _ = writeln!(out, "Cache write   {}", count(usage.get("cache_write")));
    let _ = writeln!(out, "Cost          ${cost:.4}");
    if let Some(models) = data.get("models").and_then(Value::as_object).filter(|models| !models.is_empty()) {
        let mut rows: Vec<(&String, u64, f64)> = models
            .iter()
            .map(|(name, value)| {
                let parts = value.as_array();
                (name, count(parts.and_then(|p| p.first())), amount(parts.and_then(|p| p.get(1))))
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        let _ = writeln!(out, "\nModels");
        for (name, messages, cost) in rows.into_iter().take(10) {
            let _ = writeln!(out, "  {name:<50} {messages:>6} msgs  ${cost:.4}");
        }
    }
    if let Some(tools) = data.get("tools").and_then(Value::as_object).filter(|tools| !tools.is_empty()) {
        let mut rows: Vec<(&String, u64, u64)> = tools
            .iter()
            .map(|(name, value)| {
                let parts = value.as_array();
                (name, count(parts.and_then(|p| p.first())), count(parts.and_then(|p| p.get(1))))
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        let _ = writeln!(out, "\nTools");
        for (name, calls, errors) in rows.into_iter().take(15) {
            let _ = writeln!(out, "  {name:<30} {calls:>6} calls  {errors:>4} errors");
        }
    }
    out
}
