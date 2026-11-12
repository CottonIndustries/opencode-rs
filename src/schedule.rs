//! Session schedules: prompts that wake a session later or on a cron.

use anyhow::{anyhow, Result};
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub when: Value,
    pub tz: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Value>,
    pub action: Value,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_runs: Option<u64>,
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_state: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

pub struct Scheduler {
    schedules: Mutex<Vec<Schedule>>,
}

fn path() -> PathBuf {
    crate::paths::get().storage().join("schedules.json")
}

fn load() -> Vec<Schedule> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

impl Scheduler {
    pub fn new() -> Self {
        Scheduler { schedules: Mutex::new(load()) }
    }

    fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&*self.schedules.lock().unwrap()) {
            let path = path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, text);
        }
    }

    pub fn list(&self, session_id: Option<&str>) -> Vec<Schedule> {
        let mut out: Vec<Schedule> = self
            .schedules
            .lock()
            .unwrap()
            .iter()
            .filter(|schedule| session_id.is_none_or(|session| schedule.session_id == session))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.next_due_at.cmp(&b.next_due_at).then_with(|| a.id.cmp(&b.id)));
        out
    }

    pub fn get(&self, id: &str) -> Result<Schedule> {
        self.schedules
            .lock()
            .unwrap()
            .iter()
            .find(|schedule| schedule.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("Schedule not found: {id}"))
    }

    pub async fn create(&self, session_id: &str, directory: &std::path::Path, request: &Value) -> Result<Schedule> {
        let condition = request.get("if").cloned().filter(|value| !value.is_null()).map(normalize_condition);
        let until = request.get("until").cloned().filter(|value| !value.is_null()).map(normalize_condition);
        if condition.is_some() && until.is_some() {
            return Err(anyhow!("Choose either if or until, not both"));
        }
        let prompt = request
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| anyhow!("prompt is required"))?
            .to_string();
        let tz = request
            .pointer("/when/tz")
            .and_then(Value::as_str)
            .unwrap_or("local")
            .to_string();
        let when = match request.get("when").cloned().filter(|value| !value.is_null()) {
            Some(mut when) => {
                if when.get("type").is_none() {
                    let kind = ["in", "at", "every", "cron"]
                        .into_iter()
                        .find(|key| when.get(*key).is_some())
                        .ok_or_else(|| anyhow!("when must contain in, at, every or cron"))?;
                    when["type"] = json!(kind);
                }
                when
            }
            None if until.is_some() => json!({ "type": "every", "every": "30 seconds" }),
            None => return Err(anyhow!("when is required unless until is used")),
        };
        let now = crate::util::now_ms();
        let next = next_due(&when, now, now)?;
        let ends = request.get("ends").cloned().unwrap_or(Value::Null);
        let ends_at = ends
            .get("after")
            .and_then(Value::as_str)
            .and_then(parse_duration)
            .map(|millis| now + millis as i64)
            .or_else(|| ends.get("at").and_then(Value::as_str).and_then(parse_at));
        let remaining = ends.get("times").and_then(Value::as_u64);
        let mut schedule = Schedule {
            id: crate::util::id("sch"),
            session_id: session_id.to_string(),
            name: request.get("name").and_then(Value::as_str).map(String::from),
            when,
            tz,
            condition,
            until,
            action: json!({ "type": "prompt", "prompt": prompt }),
            status: "active".into(),
            next_due_at: next,
            ends_at,
            remaining_runs: remaining,
            run_count: 0,
            last_fired_at: None,
            last_outcome: None,
            last_output: None,
            check_state: None,
            time_created: now,
            time_updated: now,
        };
        // `until` conditions are evaluated once at creation, like upstream.
        if let Some(until) = schedule.until.clone() {
            if let Ok((wake, output, state)) = check(&until, directory, None).await {
                schedule.check_state = state;
                if wake {
                    schedule.status = "done".into();
                    schedule.last_outcome = Some("fired".into());
                    schedule.last_output = output;
                }
            }
        }
        self.schedules.lock().unwrap().push(schedule.clone());
        self.save();
        Ok(schedule)
    }

    pub fn pause(&self, id: &str) -> Result<Schedule> {
        self.update(id, |schedule| {
            schedule.status = "paused".into();
            schedule.time_updated = crate::util::now_ms();
        })
    }

    pub fn resume(&self, id: &str) -> Result<Schedule> {
        self.update(id, |schedule| {
            schedule.status = "active".into();
            schedule.time_updated = crate::util::now_ms();
            if schedule.next_due_at.is_none() || schedule.next_due_at.is_some_and(|next| next < crate::util::now_ms()) {
                schedule.next_due_at = next_due(&schedule.when, crate::util::now_ms(), schedule.time_created).ok().flatten();
            }
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut schedules = self.schedules.lock().unwrap();
        let before = schedules.len();
        schedules.retain(|schedule| schedule.id != id);
        drop(schedules);
        self.save();
        Ok(before != self.schedules.lock().unwrap().len())
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Schedule)) -> Result<Schedule> {
        let mut schedules = self.schedules.lock().unwrap();
        let schedule = schedules
            .iter_mut()
            .find(|schedule| schedule.id == id)
            .ok_or_else(|| anyhow!("Schedule not found: {id}"))?;
        change(schedule);
        let schedule = schedule.clone();
        drop(schedules);
        self.save();
        Ok(schedule)
    }

    pub async fn tick(&self, app: &Arc<crate::app::App>) {
        let now = crate::util::now_ms();
        let due: Vec<Schedule> = self
            .schedules
            .lock()
            .unwrap()
            .iter()
            .filter(|schedule| schedule.status == "active" && schedule.next_due_at.is_some_and(|next| next <= now))
            .cloned()
            .collect();
        for mut schedule in due {
            let Ok(handle) = app.session(&schedule.session_id) else {
                schedule.status = "done".into();
                schedule.last_outcome = Some("failed".into());
                schedule.last_output = Some("session not found".into());
                self.replace(schedule);
                continue;
            };
            let directory = handle.with(|session| session.directory.clone());
            let condition = schedule.until.clone().or_else(|| schedule.condition.clone());
            let mut message = None;
            let mut wake = true;
            if let Some(condition) = &condition {
                match check(condition, &directory, schedule.check_state.as_deref()).await {
                    Ok((passes, output, state)) => {
                        wake = passes;
                        message = output;
                        schedule.check_state = state;
                    }
                    Err(error) => {
                        schedule.last_outcome = Some("failed".into());
                        schedule.last_output = Some(format!("{error:#}"));
                        schedule.next_due_at = Some(now + 60_000);
                        schedule.time_updated = now;
                        self.replace(schedule);
                        continue;
                    }
                }
            }
            if wake {
                let prompt = schedule
                    .action
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let prompt = match &message {
                    Some(message) if !message.trim().is_empty() => format!("{prompt}\n\n{message}"),
                    _ => prompt,
                };
                schedule.last_output = Some(prompt.clone());
                let message = crate::session::Message::user_text(prompt);
                let _ = app.spawn_prompt(&schedule.session_id, message, crate::app::PromptOptions::default());
                schedule.run_count += 1;
                schedule.last_fired_at = Some(now);
                schedule.last_outcome = Some(if now - schedule.next_due_at.unwrap_or(now) > 60_000 { "late" } else { "fired" }.into());
            } else {
                schedule.last_outcome = Some("checked".into());
            }
            if let Some(remaining) = schedule.remaining_runs.as_mut() {
                if wake {
                    *remaining = remaining.saturating_sub(1);
                }
            }
            if schedule.until.is_some() {
                if wake {
                    schedule.status = "done".into();
                    schedule.next_due_at = None;
                } else {
                    schedule.next_due_at = next_due(&schedule.when, now, schedule.time_created).ok().flatten();
                }
            } else if matches!(schedule.when.get("type").and_then(Value::as_str), Some("in") | Some("at")) {
                schedule.status = "done".into();
                schedule.next_due_at = None;
            } else {
                schedule.next_due_at = next_due(&schedule.when, now, schedule.time_created).ok().flatten();
            }
            if schedule.remaining_runs == Some(0)
                || schedule.ends_at.is_some_and(|ends| schedule.next_due_at.is_none_or(|next| next > ends))
            {
                schedule.status = "done".into();
                schedule.next_due_at = None;
            }
            schedule.time_updated = now;
            self.replace(schedule);
        }
    }

    fn replace(&self, schedule: Schedule) {
        {
            let mut schedules = self.schedules.lock().unwrap();
            if let Some(slot) = schedules.iter_mut().find(|candidate| candidate.id == schedule.id) {
                *slot = schedule;
            }
        }
        self.save();
    }
}

pub fn start(app: Arc<crate::app::App>) {
    let weak = Arc::downgrade(&app);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let Some(app) = weak.upgrade() else { return };
            app.schedules.tick(&app).await;
        }
    });
}

fn normalize_condition(mut condition: Value) -> Value {
    if condition.get("type").is_none() {
        condition["type"] = json!(if condition.get("code").is_some() { "code" } else { "shell" });
    }
    condition
}

fn parse_duration(value: &str) -> Option<u64> {
    let value = value.trim().to_lowercase();
    let mut total = 0u64;
    let mut number = String::new();
    let mut unit = String::new();
    let flush = |number: &mut String, unit: &mut String, total: &mut u64| -> bool {
        if number.is_empty() {
            unit.clear();
            return true;
        }
        if unit.is_empty() {
            return true;
        }
        let Ok(amount) = number.parse::<f64>() else { return false };
        let multiplier = match unit.as_str() {
            "ms" | "millisecond" | "milliseconds" => 1.0,
            "s" | "sec" | "second" | "seconds" => 1000.0,
            "m" | "min" | "minute" | "minutes" => 60_000.0,
            "h" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            _ => return false,
        };
        *total += (amount * multiplier) as u64;
        number.clear();
        unit.clear();
        true
    };
    for ch in value.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if !number.is_empty() && !unit.is_empty() && !flush(&mut number, &mut unit, &mut total) {
                return None;
            }
            number.push(ch);
        } else if ch.is_alphabetic() {
            unit.push(ch);
        } else if ch.is_whitespace() {
            if !unit.is_empty() && !flush(&mut number, &mut unit, &mut total) {
                return None;
            }
        } else {
            return None;
        }
    }
    if !flush(&mut number, &mut unit, &mut total) {
        return None;
    }
    (total > 0).then_some(total)
}

fn parse_at(value: &str) -> Option<i64> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(time.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(value, format) {
            if let Some(local) = chrono::TimeZone::from_local_datetime(&chrono::Local, &time).single() {
                return Some(local.timestamp_millis());
            }
        }
    }
    None
}

fn next_due(when: &Value, now: i64, created: i64) -> Result<Option<i64>> {
    match when.get("type").and_then(Value::as_str).unwrap_or("in") {
        "in" => {
            let millis = when
                .get("in")
                .and_then(Value::as_str)
                .and_then(parse_duration)
                .ok_or_else(|| anyhow!("invalid duration"))?;
            Ok(Some(now + millis as i64))
        }
        "at" => {
            let at = when
                .get("at")
                .and_then(Value::as_str)
                .and_then(parse_at)
                .ok_or_else(|| anyhow!("invalid time"))?;
            Ok((at > now).then_some(at))
        }
        "every" => {
            let millis = when
                .get("every")
                .and_then(Value::as_str)
                .and_then(parse_duration)
                .ok_or_else(|| anyhow!("invalid duration"))?;
            let base = if now > created { created } else { now };
            let slots = (now - base) / millis as i64 + 1;
            Ok(Some(base + slots * millis as i64))
        }
        "cron" => {
            let expression = when
                .get("cron")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("cron expression is required"))?;
            Ok(cron_next(expression, now))
        }
        other => Err(anyhow!("unknown schedule type: {other}")),
    }
}

async fn check(condition: &Value, directory: &std::path::Path, previous: Option<&str>) -> Result<(bool, Option<String>, Option<String>)> {
    match condition.get("type").and_then(Value::as_str) {
        Some("code") => Err(anyhow!("code conditions require Code Mode")),
        _ => {
            let run = condition
                .get("run")
                .and_then(Value::as_str)
                .filter(|run| !run.is_empty())
                .ok_or_else(|| anyhow!("condition run is required"))?;
            let shell = crate::tools::shell::detect();
            let mut command = tokio::process::Command::new(&shell.program);
            command
                .args(&shell.args)
                .arg(run)
                .current_dir(directory)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
                .await
                .map_err(|_| anyhow!("condition timed out"))??;
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let mut state = previous.map(String::from);
            let success = output.status.success();
            let wake = if let Some(expected) = condition.get("is").and_then(Value::as_str) {
                success && text == expected
            } else if let Some(pattern) = condition.get("matches").and_then(Value::as_str) {
                success && regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(&text))
            } else if condition.get("changes").and_then(Value::as_bool).unwrap_or(false) {
                success && previous != Some(text.as_str())
            } else {
                success
            };
            if condition.get("changes").and_then(Value::as_bool).unwrap_or(false) {
                state = Some(text.clone());
            }
            Ok((wake, (!text.is_empty()).then_some(text), state))
        }
    }
}

fn cron_next(expression: &str, now: i64) -> Option<i64> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let minutes = cron_field(fields[0], 0, 59)?;
    let hours = cron_field(fields[1], 0, 23)?;
    let days = cron_field(fields[2], 1, 31)?;
    let months = cron_field(fields[3], 1, 12)?;
    let weekdays = cron_field(fields[4], 0, 6)?;
    let start = chrono::Local.timestamp_millis_opt(now).single()? + chrono::Duration::minutes(1);
    let mut time = start;
    for _ in 0..(366 * 24 * 60) {
        use chrono::{Datelike, Timelike};
        let minute = time.minute() as u32;
        let hour = time.hour() as u32;
        let day = time.day();
        let month = time.month();
        let weekday = time.weekday().num_days_from_sunday();
        if minutes.contains(&minute)
            && hours.contains(&hour)
            && days.contains(&day)
            && months.contains(&month)
            && weekdays.contains(&weekday)
        {
            return Some(time.timestamp_millis());
        }
        time += chrono::Duration::minutes(1);
    }
    None
}

fn cron_field(value: &str, min: u32, max: u32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok()?),
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            (start.parse().ok()?, end.parse().ok()?)
        } else {
            let value: u32 = range.parse().ok()?;
            (value, value)
        };
        if start < min || end > max || start > end {
            return None;
        }
        out.extend((start..=end).step_by(step as usize));
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30 seconds"), Some(30_000));
        assert_eq!(parse_duration("2 hours"), Some(7_200_000));
        assert_eq!(parse_duration("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn cron_finds_the_next_slot() {
        use chrono::Timelike;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 1, 1, 8, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let next = cron_next("0 9 * * 1-5", now).unwrap();
        let next = chrono::Local.timestamp_millis_opt(next).single().unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn every_keeps_its_phase() {
        let next = next_due(&json!({ "type": "every", "every": "1 minute" }), 125_000, 0).unwrap();
        assert_eq!(next, Some(180_000));
    }
}
