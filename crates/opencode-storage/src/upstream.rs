//! Compatibility bridge for upstream opencode v2's `opencode.db` session tables.
//!
//! Upstream stores sessions in `session_v2` and messages in `session_message`.
//! This module maps the port's session model to those tables so both clients can
//! open the same sessions. The legacy JSON/sessions.db store remains the local
//! fallback when no upstream database is present.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::session::{
    FilePart, Message, Part, Role, Session, ShellMessage, Summary, ToolState, Usage,
};

fn connection() -> Option<Connection> {
    let path = crate::paths::get().db_file();
    if !path.is_file() {
        return None;
    }
    let connection = Connection::open(path).ok()?;
    let _ = connection.busy_timeout(Duration::from_secs(5));
    let _ = connection.execute_batch("PRAGMA foreign_keys = ON;");
    has_schema(&connection).then_some(connection)
}

fn has_schema(connection: &Connection) -> bool {
    let exists = |name: &str| {
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![name],
                |_| Ok(()),
            )
            .is_ok()
    };
    exists("project") && exists("session_v2") && exists("session_message")
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "session".into()
    } else {
        out
    }
}

fn model_json(model: Option<&str>) -> Option<String> {
    model.map(|model| model_value(model).to_string())
}

fn model_value(model: &str) -> Value {
    let (provider, rest) = model.split_once('/').unwrap_or(("", model));
    let (id, variant) = rest.split_once('#').unwrap_or((rest, ""));
    let mut value = json!({ "id": id, "providerID": provider });
    if !variant.is_empty() {
        value["variant"] = json!(variant);
    }
    value
}

fn model_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let provider = value
        .get("providerID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = value.get("id").and_then(Value::as_str).unwrap_or_default();
    if id.is_empty() {
        return None;
    }
    let mut out = if provider.is_empty() {
        id.to_string()
    } else {
        format!("{provider}/{id}")
    };
    if let Some(variant) = value
        .get("variant")
        .and_then(Value::as_str)
        .filter(|variant| !variant.is_empty())
    {
        out.push('#');
        out.push_str(variant);
    }
    Some(out)
}

fn usage_value(usage: &Usage) -> Value {
    json!({
        "input": usage.input,
        "output": usage.output,
        "reasoning": usage.reasoning,
        "cache": { "read": usage.cache_read, "write": usage.cache_write }
    })
}

fn usage_from(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    let number = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cache = value.get("cache");
    Usage {
        input: number("input"),
        output: number("output"),
        reasoning: number("reasoning"),
        cache_read: cache
            .and_then(|cache| cache.get("read"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write: cache
            .and_then(|cache| cache.get("write"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost: value.get("cost").and_then(Value::as_f64),
    }
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(key, value)| {
                    let value = strip_nulls(value);
                    (!value.is_null()).then_some((key, value))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        value => value,
    }
}

fn file_value(file: &FilePart) -> Value {
    json!({
        "data": file.data,
        "mime": file.mime,
        "name": file.name,
        "source": { "type": "inline" }
    })
}

fn file_from(value: &Value) -> Option<FilePart> {
    Some(FilePart {
        mime: value.get("mime").and_then(Value::as_str)?.to_string(),
        data: value.get("data").and_then(Value::as_str)?.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn tool_state_value(state: &ToolState, input: &Value) -> Value {
    match state {
        ToolState::Pending => json!({
            "status": "streaming",
            "input": input.to_string()
        }),
        ToolState::Running => json!({
            "status": "running",
            "input": input,
            "metadata": {}
        }),
        ToolState::Completed {
            output,
            metadata,
            attachments,
            ..
        } => {
            let mut content = vec![json!({ "type": "text", "text": output })];
            for attachment in attachments {
                content.push(file_value(attachment));
            }
            let mut state = json!({
                "status": "completed",
                "input": input,
                "content": content
            });
            if !metadata.is_null() {
                state["metadata"] = metadata.clone();
            }
            state
        }
        ToolState::Error { error } => json!({
            "status": "error",
            "input": input,
            "error": { "type": "unknown", "message": error }
        }),
    }
}

fn tool_state_from(value: &Value) -> ToolState {
    match value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed")
    {
        "streaming" => ToolState::Pending,
        "running" => ToolState::Running,
        "error" => ToolState::Error {
            error: value
                .get("error")
                .and_then(|error| error.get("message").or(Some(error)))
                .and_then(Value::as_str)
                .unwrap_or("tool failed")
                .to_string(),
        },
        _ => {
            let mut output = String::new();
            let mut attachments = Vec::new();
            if let Some(content) = value.get("content").and_then(Value::as_array) {
                for item in content {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                if !output.is_empty() {
                                    output.push('\n');
                                }
                                output.push_str(text);
                            }
                        }
                        Some("file") | Some("image") => {
                            if let Some(file) = file_from(item) {
                                attachments.push(file);
                            }
                        }
                        _ => {}
                    }
                }
            }
            ToolState::Completed {
                output,
                title: String::new(),
                metadata: value.get("metadata").cloned().unwrap_or(Value::Null),
                attachments,
            }
        }
    }
}

fn user_data(message: &Message) -> Value {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let files = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::File(file) => Some(file_value(file)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut data = json!({
        "time": { "created": message.time },
        "text": text,
        "files": files,
        "agents": message.agents.clone().unwrap_or_else(|| json!([])),
        "skills": message.skills.clone().unwrap_or_else(|| json!([]))
    });
    if let Some(metadata) = &message.metadata {
        data["metadata"] = metadata.clone();
    }
    data
}

fn assistant_content(message: &Message) -> Vec<Value> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
            Part::Reasoning {
                text,
                started,
                ended,
                ..
            } => Some(json!({
                "type": "reasoning",
                "text": text,
                "time": {
                    "created": started.unwrap_or(message.time),
                    "completed": ended
                }
            })),
            Part::Tool {
                id,
                name,
                input,
                state,
                provider_meta,
            } => {
                let mut tool = json!({
                    "type": "tool",
                    "id": id,
                    "name": name,
                    "executed": false,
                    "state": tool_state_value(state, input),
                    "time": { "created": message.time }
                });
                if let Some(native) = provider_meta {
                    tool["native"] = native.clone();
                }
                Some(tool)
            }
            Part::File(_) | Part::Compaction { .. } => None,
        })
        .collect()
}

fn assistant_data(message: &Message) -> Value {
    let mut data = json!({
        "time": {
            "created": message.time,
            "completed": message.completed
        },
        "agent": message.agent.clone().unwrap_or_else(|| "build".into()),
        "model": model_value(message.model.as_deref().unwrap_or("unknown/unknown")),
        "content": assistant_content(message),
        "cost": message.cost,
        "tokens": usage_value(&message.usage)
    });
    if let Some(finish) = &message.finish {
        data["finish"] = json!(finish);
    }
    if let Some(error) = &message.error {
        data["error"] = json!({ "type": "unknown", "message": error });
    }
    if let Some(metadata) = &message.metadata {
        data["metadata"] = metadata.clone();
    }
    if let Some(snapshot) = &message.snapshot {
        data["snapshot"] = json!({ "start": snapshot });
    }
    data
}

fn compaction_data(message: &Message, summary: &str, metadata: Option<&Value>) -> Value {
    let mut data = json!({
        "status": "completed",
        "reason": "manual",
        "summary": summary,
        "time": { "created": message.time }
    });
    if let Some(metadata) = metadata {
        if let Some(status) = metadata.get("status").and_then(Value::as_str) {
            data["status"] = json!(status);
        }
        if let Some(reason) = metadata.get("reason").and_then(Value::as_str) {
            data["reason"] = json!(reason);
        }
        if let Some(tokens) = metadata.get("tokens") {
            data["tokens"] = tokens.clone();
        }
    }
    data
}

fn shell_data(shell: &ShellMessage) -> Value {
    let output = shell.output.clone().unwrap_or_default();
    let size = output.len();
    let status = match shell.status.as_str() {
        "completed" | "done" => "exited",
        other => other,
    };
    json!({
        "time": { "created": shell.started, "completed": shell.completed },
        "shellID": shell.id,
        "command": shell.command,
        "status": status,
        "exit": shell.exit,
        "output": {
            "output": output,
            "cursor": size,
            "size": size,
            "truncated": false
        }
    })
}

fn ensure_project(connection: &Connection, session: &Session) -> Result<String> {
    let project = crate::session::upstream_project_id(&session.directory);
    let worktree = crate::util::normalize(&session.directory)
        .to_string_lossy()
        .replace('\\', "/");
    let now = crate::util::now_ms();
    connection.execute(
        "INSERT INTO project (id, worktree, vcs, name, sandboxes, time_created, time_updated, time_active)
         VALUES (?1, ?2, NULL, NULL, '[]', ?3, ?4, 0)
         ON CONFLICT(id) DO UPDATE SET worktree = excluded.worktree, time_updated = excluded.time_updated",
        params![project, worktree, now, now],
    )?;
    Ok(project)
}

fn save_connection(connection: &mut Connection, session: &Session) -> Result<()> {
    let project = ensure_project(connection, session)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM session_message WHERE session_id = ?1",
        params![session.id],
    )?;
    let (fork_id, fork_boundary) = session
        .fork
        .as_ref()
        .map(|fork| {
            (
                fork.get("id")
                    .or_else(|| fork.get("sessionID"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                fork.get("boundary")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        })
        .unwrap_or((None, None));
    transaction.execute(
        "INSERT INTO session_v2
         (id, project_id, workspace_id, parent_id, fork_session_id, fork_boundary, slug, directory, title, version,
          metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
          revert, permission, agent, model, time_created, time_updated, time_idle, time_viewed, idle_outcome,
          time_archived, resume_attempts, time_busy)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, 0, ?28)
         ON CONFLICT(id) DO UPDATE SET
           project_id = excluded.project_id, workspace_id = excluded.workspace_id, parent_id = excluded.parent_id,
           fork_session_id = excluded.fork_session_id, fork_boundary = excluded.fork_boundary, slug = excluded.slug,
           directory = excluded.directory, title = excluded.title, version = excluded.version, metadata = excluded.metadata,
           cost = excluded.cost, tokens_input = excluded.tokens_input, tokens_output = excluded.tokens_output,
           tokens_reasoning = excluded.tokens_reasoning, tokens_cache_read = excluded.tokens_cache_read,
           tokens_cache_write = excluded.tokens_cache_write, revert = excluded.revert, permission = excluded.permission,
           agent = excluded.agent, model = excluded.model, time_created = excluded.time_created,
           time_updated = excluded.time_updated, time_idle = excluded.time_idle, time_viewed = excluded.time_viewed,
           idle_outcome = excluded.idle_outcome, time_archived = excluded.time_archived, time_busy = excluded.time_busy",
        params![
            session.id,
            project,
            session.workspace_id,
            session.parent_id,
            fork_id,
            fork_boundary,
            slug(&session.title),
            session.directory.to_string_lossy().replace('\\', "/"),
            session.title,
            env!("CARGO_PKG_VERSION"),
            session.metadata.as_ref().map(Value::to_string),
            session.cost_total,
            session.tokens_total.input as i64,
            session.tokens_total.output as i64,
            session.tokens_total.reasoning as i64,
            session.tokens_total.cache_read as i64,
            session.tokens_total.cache_write as i64,
            session.revert.as_ref().map(Value::to_string),
            session.permissions.as_ref().map(Value::to_string),
            session.agent,
            model_json(session.model.as_deref()),
            session.time_created,
            session.time_updated,
            session.idle_ms,
            session.viewed_at,
            session.last_outcome,
            session.archived.then_some(session.time_updated),
            session.busy_at,
        ],
    )?;
    let mut seq = 0i64;
    let mut ids = HashSet::new();
    let mut insert = |id: String,
                      kind: &str,
                      created: i64,
                      updated: i64,
                      data: Value|
     -> Result<()> {
        let mut id = id;
        if id.is_empty() || !id.starts_with("msg_") {
            id = format!("msg_rs_{seq:08}");
        }
        while !ids.insert(id.clone()) {
            id.push('_');
        }
        transaction.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, session.id, kind, seq, created, updated, strip_nulls(data).to_string()],
        )?;
        seq += 1;
        Ok(())
    };
    for message in &session.messages {
        if let Some((summary, metadata)) = message.parts.iter().find_map(|part| match part {
            Part::Compaction { summary, metadata } => Some((summary.as_str(), metadata.as_ref())),
            _ => None,
        }) {
            insert(
                message.id.clone(),
                "compaction",
                message.time,
                message.completed.unwrap_or(message.time),
                compaction_data(message, summary, metadata),
            )?;
            continue;
        }
        if message.role == Role::User {
            insert(
                message.id.clone(),
                "user",
                message.time,
                message.time,
                user_data(message),
            )?;
        } else {
            insert(
                message.id.clone(),
                "assistant",
                message.time,
                message.completed.unwrap_or(message.time),
                assistant_data(message),
            )?;
        }
    }
    for shell in &session.shells {
        insert(
            shell.id.clone(),
            "shell",
            shell.started,
            shell.completed.unwrap_or(shell.started),
            shell_data(shell),
        )?;
    }
    for record in &session.records {
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("system");
        let created = record
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(session.time_updated);
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut data = record.clone();
        if let Some(map) = data.as_object_mut() {
            map.remove("type");
            map.remove("id");
        }
        insert(id, kind, created, created, data)?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn save(session: &Session) {
    let Some(mut connection) = connection() else {
        return;
    };
    let _ = save_connection(&mut connection, session);
}

fn load_connection(connection: &Connection, id: &str) -> Result<Option<Session>> {
    let row = connection
        .query_row(
            "SELECT id, workspace_id, parent_id, fork_session_id, fork_boundary, directory, title, metadata,
                    cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
                    revert, permission, agent, model, time_created, time_updated, time_idle, time_viewed,
                    idle_outcome, time_archived, time_busy
             FROM session_v2 WHERE id = ?1",
            params![id],
            |row| {
                let text = |index: usize| row.get::<_, Option<String>>(index);
                Ok((
                    row.get::<_, String>(0)?,
                    text(1)?,
                    text(2)?,
                    text(3)?,
                    text(4)?,
                    row.get::<_, String>(5)?,
                    text(6)?,
                    text(7)?,
                    row.get::<_, Option<f64>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    text(14)?,
                    text(15)?,
                    text(16)?,
                    text(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                    row.get::<_, Option<i64>>(20)?,
                    row.get::<_, Option<i64>>(21)?,
                    text(22)?,
                    row.get::<_, Option<i64>>(23)?,
                    row.get::<_, Option<i64>>(24)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else { return Ok(None) };
    let directory = PathBuf::from(row.5);
    let metadata = row
        .7
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let revert = row
        .14
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let permissions = row
        .15
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let fork = row.3.as_ref().map(|id| {
        let mut fork = json!({ "id": id });
        if let Some(boundary) = &row.4 {
            fork["boundary"] = json!(boundary);
        }
        fork
    });
    let model = row
        .17
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let mut session = Session {
        id: row.0,
        project: crate::session::project_id(&directory),
        directory,
        workspace_id: row.1,
        title: row.6.unwrap_or_default(),
        parent_id: row.2,
        time_created: row.18,
        time_updated: row.19,
        agent: row.16,
        model: model_string(model.as_ref()),
        messages: vec![],
        records: vec![],
        shells: vec![],
        todos: vec![],
        approved: vec![],
        loaded_instructions: vec![],
        metadata,
        permissions,
        revert,
        environment: Default::default(),
        viewed_at: row.21,
        idle_ms: row.20,
        busy_at: row.24,
        last_outcome: row.22,
        fork,
        archived: row.23.is_some(),
        instruction_entries: Default::default(),
        cost_total: row.8.unwrap_or(0.0),
        tokens_total: Usage {
            input: row.9.max(0) as u64,
            output: row.10.max(0) as u64,
            reasoning: row.11.max(0) as u64,
            cache_read: row.12.max(0) as u64,
            cache_write: row.13.max(0) as u64,
            cost: None,
        },
    };
    let mut statement = connection.prepare(
        "SELECT id, type, time_created, time_updated, data FROM session_message WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = statement.query_map(params![id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (message_id, kind, created, updated, text) = row?;
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match kind.as_str() {
            "user" => {
                let mut message = Message::new(Role::User);
                message.id = message_id;
                message.time = data
                    .get("time")
                    .and_then(|time| time.get("created"))
                    .and_then(Value::as_i64)
                    .unwrap_or(created);
                message.completed = Some(updated);
                message.metadata = data.get("metadata").cloned();
                message.agents = data.get("agents").cloned();
                message.skills = data.get("skills").cloned();
                if let Some(text) = data.get("text").and_then(Value::as_str) {
                    message.parts.push(Part::Text {
                        text: text.to_string(),
                        synthetic: false,
                    });
                }
                if let Some(files) = data.get("files").and_then(Value::as_array) {
                    for file in files {
                        if let Some(file) = file_from(file) {
                            message.parts.push(Part::File(file));
                        }
                    }
                }
                session.messages.push(message);
            }
            "assistant" => {
                let mut message = Message::new(Role::Assistant);
                message.id = message_id;
                message.time = data
                    .get("time")
                    .and_then(|time| time.get("created"))
                    .and_then(Value::as_i64)
                    .unwrap_or(created);
                message.completed = data
                    .get("time")
                    .and_then(|time| time.get("completed"))
                    .and_then(Value::as_i64)
                    .or(Some(updated));
                message.agent = data
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                message.model = model_string(data.get("model"));
                message.usage = usage_from(data.get("tokens"));
                message.cost = data.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                message.finish = data
                    .get("finish")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                message.error = data.get("error").and_then(error_string);
                message.metadata = data.get("metadata").cloned();
                message.snapshot = data
                    .get("snapshot")
                    .and_then(|snapshot| snapshot.get("start"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(content) = data.get("content").and_then(Value::as_array) {
                    for item in content {
                        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                            "text" => {
                                if let Some(text) = item.get("text").and_then(Value::as_str) {
                                    message.parts.push(Part::Text {
                                        text: text.to_string(),
                                        synthetic: false,
                                    });
                                }
                            }
                            "reasoning" => {
                                let started = item
                                    .get("time")
                                    .and_then(|time| time.get("created"))
                                    .and_then(Value::as_i64);
                                let ended = item
                                    .get("time")
                                    .and_then(|time| time.get("completed"))
                                    .and_then(Value::as_i64);
                                message.parts.push(Part::Reasoning {
                                    text: item
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    signature: None,
                                    redacted: None,
                                    item_id: None,
                                    started,
                                    ended,
                                });
                            }
                            "tool" => {
                                message.parts.push(Part::Tool {
                                    id: item
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    name: item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    input: item
                                        .get("state")
                                        .and_then(|state| state.get("input"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    state: item
                                        .get("state")
                                        .map(tool_state_from)
                                        .unwrap_or(ToolState::Pending),
                                    provider_meta: item.get("native").cloned(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                session.messages.push(message);
            }
            "compaction" => {
                let mut message = Message::new(Role::Assistant);
                message.id = message_id;
                message.time = data
                    .get("time")
                    .and_then(|time| time.get("created"))
                    .and_then(Value::as_i64)
                    .unwrap_or(created);
                message.completed = Some(updated);
                message.parts.push(Part::Compaction {
                    summary: data
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    metadata: Some(data.clone()),
                });
                session.messages.push(message);
            }
            "shell" => {
                session.shells.push(ShellMessage {
                    id: data
                        .get("shellID")
                        .and_then(Value::as_str)
                        .unwrap_or(&message_id)
                        .to_string(),
                    command: data
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    status: data
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("exited")
                        .to_string(),
                    exit: data.get("exit").and_then(Value::as_i64),
                    output: data
                        .get("output")
                        .and_then(|output| output.get("output").or(Some(output)))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    started: created,
                    completed: data
                        .get("time")
                        .and_then(|time| time.get("completed"))
                        .and_then(Value::as_i64),
                });
            }
            _ => {
                let mut record = data;
                record["type"] = json!(kind);
                if record.get("id").is_none() {
                    record["id"] = json!(message_id);
                }
                session.records.push(record);
            }
        }
    }
    Ok(Some(session))
}

fn error_string(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .map(str::to_string)
}

pub fn load(id: &str) -> Option<Session> {
    let connection = connection()?;
    load_connection(&connection, id).ok().flatten()
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Summary> {
    let directory = PathBuf::from(row.get::<_, String>(6)?);
    let metadata = row
        .get::<_, Option<String>>(7)?
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let permissions = row
        .get::<_, Option<String>>(8)?
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let revert = row
        .get::<_, Option<String>>(9)?
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok());
    let fork = row
        .get::<_, Option<String>>(10)?
        .map(|id| json!({ "id": id }));
    Ok(Summary {
        id: row.get(0)?,
        title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        parent_id: row.get(2)?,
        time_created: row.get(3)?,
        time_updated: row.get(4)?,
        directory,
        workspace_id: row.get(5)?,
        metadata,
        permissions,
        revert,
        viewed_at: row.get(11)?,
        idle_ms: row.get(12)?,
        busy_at: row.get(13)?,
        last_outcome: row.get(14)?,
        fork,
        archived: row.get::<_, Option<i64>>(15)?.is_some(),
        cost: row.get::<_, Option<f64>>(16)?.unwrap_or(0.0),
        tokens: Usage {
            input: row.get::<_, i64>(17)?.max(0) as u64,
            output: row.get::<_, i64>(18)?.max(0) as u64,
            reasoning: row.get::<_, i64>(19)?.max(0) as u64,
            cache_read: row.get::<_, i64>(20)?.max(0) as u64,
            cache_write: row.get::<_, i64>(21)?.max(0) as u64,
            cost: None,
        },
    })
}

pub fn summaries(project: &str) -> Vec<Summary> {
    let Some(connection) = connection() else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, title, parent_id, time_created, time_updated, workspace_id, directory, metadata, permission, revert,
                fork_session_id, time_viewed, time_idle, time_busy, idle_outcome, time_archived, cost,
                tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write
         FROM session_v2 ORDER BY time_updated DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], summary_row) else {
        return Vec::new();
    };
    rows.flatten()
        .filter(|summary| crate::session::project_id(&summary.directory) == project)
        .collect()
}

pub fn delete(id: &str) -> bool {
    let Some(connection) = connection() else {
        return false;
    };
    let _ = connection.execute(
        "DELETE FROM session_message WHERE session_id = ?1",
        params![id],
    );
    connection
        .execute("DELETE FROM session_v2 WHERE id = ?1", params![id])
        .is_ok_and(|rows| rows > 0)
}

fn updated_connection(connection: &Connection, id: &str) -> Option<i64> {
    connection
        .query_row(
            "SELECT time_updated FROM session_v2 WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

pub fn updated(id: &str) -> Option<i64> {
    updated_connection(&connection()?, id)
}

/// Mirror local JSON sessions that upstream does not have yet, or that are
/// newer than the upstream row. This makes existing opencode-rs sessions
/// visible to the TypeScript TUI without requiring the session to be reopened.
pub fn sync_local() -> usize {
    let Some(mut connection) = connection() else {
        return 0;
    };
    let root = crate::paths::get().storage().join("session");
    let Ok(projects) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut synced = 0;
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            if file.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = file.path().file_stem().and_then(|name| name.to_str()).map(str::to_string) else {
                continue;
            };
            // The index knows the local timestamp, so an already-synced large
            // transcript is skipped without reading or parsing its JSON.
            if let Some(local_updated) = crate::db::updated(&id) {
                if updated_connection(&connection, &id).is_some_and(|updated| updated >= local_updated) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(file.path()) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&text) else {
                continue;
            };
            if updated_connection(&connection, &session.id)
                .is_some_and(|updated| updated >= session.time_updated)
            {
                continue;
            }
            if save_connection(&mut connection, &session).is_ok() {
                synced += 1;
            }
        }
    }
    synced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE project (
                    id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT, icon_url TEXT,
                    icon_url_override TEXT, icon_color TEXT, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, time_initialized INTEGER, time_active INTEGER NOT NULL DEFAULT 0,
                    time_archived INTEGER, sandboxes TEXT NOT NULL, commands TEXT
                );
                CREATE TABLE session_v2 (
                    id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT,
                    fork_session_id TEXT, fork_boundary TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                    path TEXT, title TEXT, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER,
                    summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, metadata TEXT,
                    cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
                    tokens_output INTEGER NOT NULL DEFAULT 0, tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_read INTEGER NOT NULL DEFAULT 0, tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                    revert TEXT, permission TEXT, agent TEXT, model TEXT, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, time_idle INTEGER, time_viewed INTEGER, idle_outcome TEXT,
                    time_compacting INTEGER, time_archived INTEGER, time_suspended INTEGER,
                    resume_attempts INTEGER NOT NULL DEFAULT 0, time_busy INTEGER
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );",
            )
            .unwrap();
    }

    #[test]
    fn round_trips_session_and_messages() {
        let mut connection = Connection::open_in_memory().unwrap();
        schema(&connection);
        let directory = std::env::temp_dir();
        let mut session = Session::new("test", &directory, None);
        session.title = "Shared session".into();
        session.model = Some("opencode/deepseek#max".into());
        session.agent = Some("build".into());
        let mut user = Message::user_text("hello");
        user.id = "msg_user".into();
        session.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.id = "msg_assistant".into();
        assistant.agent = Some("build".into());
        assistant.model = Some("opencode/deepseek#max".into());
        assistant.completed = Some(assistant.time + 5);
        assistant.usage.input = 10;
        assistant.usage.output = 4;
        assistant.parts.push(Part::Text {
            text: "hi".into(),
            synthetic: false,
        });
        assistant.parts.push(Part::Tool {
            id: "call_1".into(),
            name: "shell".into(),
            input: json!({ "command": "echo hi" }),
            state: ToolState::Completed {
                output: "hi".into(),
                title: "shell".into(),
                metadata: Value::Null,
                attachments: vec![],
            },
            provider_meta: None,
        });
        session.messages.push(assistant);
        save_connection(&mut connection, &session).unwrap();
        let loaded = load_connection(&connection, &session.id).unwrap().unwrap();
        assert_eq!(loaded.title, session.title);
        assert_eq!(loaded.model.as_deref(), Some("opencode/deepseek#max"));
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].text(), "hi");
        assert!(matches!(
            &loaded.messages[1].parts[1],
            Part::Tool { name, state: ToolState::Completed { output, .. }, .. } if name == "shell" && output == "hi"
        ));
        let summaries = {
            let mut statement = connection
                .prepare("SELECT id, title, parent_id, time_created, time_updated, workspace_id, directory, metadata, permission, revert, fork_session_id, time_viewed, time_idle, time_busy, idle_outcome, time_archived, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write FROM session_v2")
                .unwrap();
            statement
                .query_map([], summary_row)
                .unwrap()
                .flatten()
                .collect::<Vec<_>>()
        };
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session.id);
    }
}
