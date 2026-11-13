//! The agent loop: stream a model turn, execute tool calls, repeat until the
//! model stops. Also handles queued steering messages, max steps, output-length
//! recovery, automatic compaction and session titles.

use crate::agent::Agent;
use crate::app::{App, AppEvent, Handle, PromptOptions};
use crate::models::{Model, Provider};
use crate::provider::{self, Event, Request, ToolSpec};
use crate::session::{Message, Part, Role, ToolState, Usage};
use crate::tools::{self, Tool};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const REJECTED: &str = "The user rejected permission to use this tool. Stop and wait for the user's next instruction; do not retry.";

pub const COMPACTION_TEMPLATE: &str = "You MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Requirements
- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]

## Decisions
- [decisions already made and why, or \"(none)\"]

## Work State
Break the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.
### Completed
- [goals that have been completed; otherwise \"(none)\"]

### Active
- [goals currently being worked on; otherwise \"(none)\"]

### Blocked
- [anything blocking progress, and why; otherwise \"(none)\"]

## Next Move
1. [ordered list of next actions, or \"(none)\"]

## Relevant Files
List the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".
- `[file or directory path]`: [brief reason it matters]

## Important Context
- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]
</template>";

const COMPACTION_RULES: &str = "Rules:
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.
- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.
- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.
- Do not mention the summary process or that context was compacted.";

fn compaction_prompt(existing: bool) -> String {
    let common = [
        "Summarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.",
        COMPACTION_TEMPLATE,
        COMPACTION_RULES,
        "Do not continue the task or call tools.",
        "Return only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.",
    ];
    let mut parts: Vec<&str> = Vec::new();
    if existing {
        parts.push("Update the existing checkpoint in the conversation above into one consolidated summary.");
        parts.push("Newer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.");
        parts.push("Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.");
        parts.push("Return only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.");
    } else {
        parts.push("You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.");
    }
    parts.extend(common);
    parts.join("\n\n")
}

#[derive(Clone)]
pub struct Selection {
    pub provider: Provider,
    pub model: Model,
    pub variant: Option<String>,
}

/// Resolve the model for a turn: explicit option, agent, session, then default.
pub fn select_model(app: &App, agent: &Agent, opts: &PromptOptions, session_model: Option<&str>) -> Result<Selection> {
    let reference = opts
        .model
        .clone()
        .or_else(|| agent.model.clone())
        .or_else(|| session_model.map(String::from))
        .or_else(|| app.default_model())
        .ok_or_else(|| anyhow!("No model available. Set an API key (e.g. ANTHROPIC_API_KEY or OPENAI_API_KEY), run `opencode auth login`, or set \"model\" in opencode.json."))?;
    let r = crate::models::resolve(&app.providers.read().unwrap(), &reference)?;
    let variant = opts.variant.clone().or(r.variant).or_else(|| agent.variant.clone());
    if r.provider.key.is_none() && !app.config().provider.contains_key(&r.provider.id) && r.provider.id != "opencode" {
        let env = r.provider.env.first().cloned().unwrap_or_else(|| format!("{}_API_KEY", r.provider.id.to_uppercase()));
        return Err(anyhow!("No credentials for provider {}. Set {env} or run `opencode auth login {}`.", r.provider.id, r.provider.id));
    }
    Ok(Selection { provider: r.provider, model: r.model, variant })
}

pub fn cost(model: &Model, u: &Usage) -> f64 {
    // catalog pricing is only the fallback for endpoints that don't report what they charged
    if let Some(c) = u.cost {
        return c;
    }
    (u.input as f64 * model.cost_input + u.output as f64 * model.cost_output + u.cache_read as f64 * model.cost_cache_read + u.cache_write as f64 * model.cost_cache_write) / 1_000_000.0
}

/// Send a user message; if the session is busy the message is queued and
/// picked up at the next step (steering).
pub async fn prompt(app: Arc<App>, session_id: &str, mut msg: Message, mut opts: PromptOptions) -> Result<()> {
    let handle = app.session(session_id)?;
    prepare_prompt(&app, &handle, &mut msg, &mut opts).await;
    let synthetic = msg.parts.iter().any(|part| matches!(part, Part::Text { synthetic: true, .. }));
    if !synthetic && msg.snapshot.is_none() {
        msg.snapshot = crate::snapshot::capture(&app.worktree).map(|snapshot| snapshot.id);
    }
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.inbox.enqueued",
        "properties": {
            "sessionID": session_id,
            "inboxID": msg.id.clone(),
            "item": inbox_item(&msg, opts.delivery.as_deref().unwrap_or("steer")),
        },
    }));
    if handle.busy.swap(true, Ordering::SeqCst) {
        handle.queue.lock().unwrap().push_back((msg, opts));
        app.emit(AppEvent::Updated { session: session_id.to_string() });
        return Ok(());
    }
    app.emit(AppEvent::Busy { session: session_id.to_string(), busy: true });
    handle.with(|session| {
        session.busy_at = Some(crate::util::now_ms());
        session.time_updated = crate::util::now_ms();
    });
    handle.save();
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.execution.started",
        "properties": { "sessionID": session_id },
    }));
    let mut result: Result<bool> = Ok(false);
    let mut pending = Some((msg, opts));
    loop {
        if let Some((msg, opts)) = pending.take() {
            app.plugins.emit(serde_json::json!({
                "id": crate::util::id("evt"),
                "type": "session.inbox.delivered",
                "properties": { "sessionID": session_id, "inboxID": msg.id },
            }));
            result = turn(app.clone(), handle.clone(), msg, opts).await;
            if let Err(e) = &result {
                app.emit(AppEvent::Error { session: session_id.to_string(), message: e.to_string() });
            }
        }
        let next = handle.queue.lock().unwrap().pop_front();
        match next {
            Some(n) => pending = Some(n),
            None => {
                handle.busy.store(false, Ordering::SeqCst);
                // re-check for a message queued between the pop and the store
                let late = handle.queue.lock().unwrap().pop_front();
                match late {
                    Some(n) if !handle.busy.swap(true, Ordering::SeqCst) => pending = Some(n),
                    Some(n) => {
                        handle.queue.lock().unwrap().push_front(n);
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    let pending_move = handle.pending_move.lock().unwrap().take();
    if let Some(directory) = pending_move {
        crate::plugin::kill_terminal(session_id);
        crate::provider::ws_transport::close(session_id).await;
        handle.with(|session| {
            let previous = session.directory.clone();
            session.record(serde_json::json!({
                "type": "location-switched",
                "location": { "directory": directory.to_string_lossy() },
                "projectID": crate::session::project_id(&directory),
                "previous": { "location": { "directory": previous.to_string_lossy() } },
            }));
            session.directory = directory.clone();
            session.project = crate::session::project_id(&directory);
        });
        handle.save();
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.moved",
            "properties": {
                "sessionID": session_id,
                "projectID": crate::session::project_id(&directory),
                "location": { "directory": directory.to_string_lossy() }
            }
        }));
    }
    // V2 experimental.idle runs before the session settles. It may admit
    // synthetic input through ctx.session.synthetic(), which starts a new turn.
    // It does not run after a failed or interrupted turn.
    if matches!(&result, Ok(false)) {
        let _ = app
            .plugins
            .trigger(
                "session",
                "experimental.idle",
                serde_json::json!({ "sessionID": session_id, "background": [] }),
            )
            .await;
    }
    // An idle hook may admit synthetic input, which starts another turn. Give
    // that admission a moment to mark the session busy before going idle.
    if let Ok(handle) = app.session(session_id) {
        for _ in 0..25 {
            if handle.is_busy() || !handle.queue.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    let busy = app.session(session_id).map(|handle| handle.is_busy() || !handle.queue.lock().unwrap().is_empty()).unwrap_or(false);
    if !busy {
        let outcome = match &result {
            Ok(false) => "succeeded",
            Ok(true) => "interrupted",
            Err(_) => "failed",
        };
        handle.with(|session| {
            session.idle_ms = Some(crate::util::now_ms());
            session.last_outcome = Some(outcome.to_string());
            session.time_updated = crate::util::now_ms();
            let mut record = serde_json::json!({ "type": "idle", "outcome": outcome });
            if let Err(error) = &result {
                record["error"] = serde_json::json!({ "type": "unknown", "message": error.to_string() });
            }
            session.record(record);
        });
        handle.save();
        app.emit(AppEvent::Busy { session: session_id.to_string(), busy: false });
        if let Some(inbox_id) = handle.pending_compaction.lock().unwrap().take() {
            app.plugins.emit(serde_json::json!({
                "id": crate::util::id("evt"),
                "type": "session.inbox.delivered",
                "properties": { "sessionID": session_id, "inboxID": inbox_id }
            }));
            let app = app.clone();
            let session = session_id.to_string();
            tokio::spawn(async move {
                let _ = compact_session(app, &session).await;
            });
        }
    }
    match &result {
        Ok(false) => app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.execution.succeeded",
            "properties": { "sessionID": session_id },
        })),
        Ok(true) => app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.execution.interrupted",
            "properties": { "sessionID": session_id, "reason": "user" },
        })),
        // A failed turn already emits AppEvent::Error, which becomes the V2
        // session.execution.failed event. Do not emit a second failure here.
        Err(_) => {}
    }
    result.map(|_| ())
}

const PLAN_LEAVE: &str = "<system-reminder>\nYou are NO LONGER in Plan mode. The previous Plan restrictions no longer apply. Any Plan mode instructions from earlier in this conversation are no longer active.\n</system-reminder>";

fn plan_enter() -> String {
    let dir = crate::paths::get().home.join(".opencode").join("plan");
    format!(
        "<system-reminder>\nYou are in Plan mode. Discuss the plan with the user directly in the conversation. Do not create or update plan files unless the user explicitly asks you to; when they do, write them only in:\n{}\n\nDo not modify any other files or ask a subagent to do so.\n\nYou remain in Plan mode until the user switches agents. If the user asks you to implement changes, do not do so. Tell them they need to switch agents.\n</system-reminder>",
        dir.display()
    )
}

/// Upstream's Plan plugin: remind the model when it enters or leaves Plan mode.
fn plan_reminder(agent: &str, history: &[Message]) -> Option<String> {
    let enter = plan_enter();
    let last = history.iter().flat_map(|m| m.parts.iter()).rev().find_map(|p| match p {
        Part::Text { text, synthetic: true } if *text == enter || text == PLAN_LEAVE => Some(text.clone()),
        _ => None,
    });
    let active = last.as_deref() == Some(enter.as_str());
    if agent == "plan" && !active {
        return Some(enter);
    }
    if agent != "plan" && active {
        return Some(PLAN_LEAVE.into());
    }
    None
}

pub(crate) fn inbox_item(msg: &Message, delivery: &str) -> Value {
    let files: Vec<Value> = msg
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::File(file) => Some(serde_json::json!({
                "data": file.data,
                "mime": file.mime,
                "source": { "type": "inline" },
                "name": file.name,
            })),
            _ => None,
        })
        .collect();
    let synthetic = msg.parts.iter().all(|part| matches!(part, Part::Text { synthetic: true, .. }));
    let mut payload = serde_json::json!({ "text": msg.text() });
    if !files.is_empty() {
        payload["files"] = Value::Array(files);
    }
    if let Some(metadata) = &msg.metadata {
        payload["metadata"] = metadata.clone();
    }
    serde_json::json!({
        "type": if synthetic { "synthetic" } else { "user" },
        "payload": payload,
        "delivery": delivery,
    })
}

/// Deliver a background notification (finished shell/subagent) to a session.
pub async fn notify(app: Arc<App>, session_id: String, text: String) {
    notify_with_id(app, session_id, text, None, None).await
}

pub async fn notify_with_id(
    app: Arc<App>,
    session_id: String,
    text: String,
    id: Option<String>,
    metadata: Option<Value>,
) {
    if let Some(id) = &id {
        crate::job::complete_background(id);
    }
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.synthetic",
        "properties": { "sessionID": session_id.clone(), "text": text.clone() },
    }));
    let mut msg = Message::new(Role::User);
    if let Some(id) = id {
        msg.id = id;
    }
    msg.metadata = metadata;
    msg.parts.push(Part::Text { text, synthetic: true });
    let opts = app
        .session(&session_id)
        .ok()
        .map(|h| h.with(|s| PromptOptions { agent: s.agent.clone(), model: s.model.clone(), variant: None, delivery: None }))
        .unwrap_or_default();
    let _ = prompt(app, &session_id, msg, opts).await;
}

/// Run the admission-time `session.prompt` hook before a message is queued.
async fn prepare_prompt(
    app: &Arc<App>,
    handle: &Arc<Handle>,
    msg: &mut Message,
    opts: &mut PromptOptions,
) {
    let sid = handle.id();
    let hooked = crate::plugin::messages_for_hook(&sid, std::slice::from_ref(msg));
    let mut prompt_event = serde_json::json!({
        "sessionID": sid,
        "messageID": msg.id,
        "prompt": {
            "text": msg.text(),
            "files": msg.parts.iter().filter_map(|part| match part {
                Part::File(file) => Some(serde_json::json!({
                    "uri": format!("data:{};base64,{}", file.mime, file.data),
                    "name": file.name,
                    "source": { "type": "inline" },
                })),
                _ => None,
            }).collect::<Vec<_>>(),
            "agents": msg.agents.clone().unwrap_or_else(|| serde_json::json!([])),
            "skills": msg.skills.clone().unwrap_or_else(|| serde_json::json!([])),
        },
        "metadata": msg.metadata.clone().unwrap_or_else(|| serde_json::json!({})),
        "delivery": opts.delivery.clone().unwrap_or_else(|| "steer".into()),
    });
    if let Some(first) = hooked.as_array().and_then(|a| a.first()) {
        prompt_event["message"] = first.get("info").cloned().unwrap_or(Value::Null);
        prompt_event["parts"] = first.get("parts").cloned().unwrap_or(Value::Null);
    }
    let prompt_event = app.plugins.trigger("session", "prompt", prompt_event).await;
    if let Some(metadata) = prompt_event.get("metadata") {
        msg.metadata = (!metadata.is_null()).then(|| metadata.clone());
    }
    if let Some(delivery) = prompt_event.get("delivery").and_then(Value::as_str) {
        opts.delivery = Some(delivery.to_string());
    }
    if let Some(agents) = prompt_event.pointer("/prompt/agents") {
        msg.agents = (!agents.is_null()).then(|| agents.clone());
    }
    if let Some(skills) = prompt_event.pointer("/prompt/skills") {
        msg.skills = (!skills.is_null()).then(|| skills.clone());
    }
    // A hook may return a prompt object without `text` (for example when it
    // only adds files or metadata). Only replace the text when the hook
    // actually supplied a string; otherwise the original user text survives.
    if let Some(prompt_text) = prompt_event.pointer("/prompt/text").and_then(Value::as_str) {
        // An empty text edit from a hook must not erase the user's message.
        if !prompt_text.is_empty() && prompt_text != msg.text() {
            let files: Vec<Part> = msg.parts.iter().filter(|p| matches!(p, Part::File(_))).cloned().collect();
            let mut parts = vec![Part::Text { text: prompt_text.to_string(), synthetic: false }];
            parts.extend(files);
            msg.parts = parts;
        }
    }
    if let Some(files) = prompt_event.pointer("/prompt/files").and_then(Value::as_array) {
        let mut parts: Vec<Part> = msg.parts.iter().filter(|p| !matches!(p, Part::File(_))).cloned().collect();
        for file in files {
            let uri = file.get("uri").and_then(Value::as_str).unwrap_or_default();
            if let Some((meta, data)) = uri.split_once(";base64,") {
                parts.push(Part::File(crate::session::FilePart {
                    mime: meta.trim_start_matches("data:").to_string(),
                    data: data.to_string(),
                    name: file.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                }));
            }
        }
        msg.parts = parts;
    }
    if let (Some(info), Some(parts)) = (prompt_event.get("message"), prompt_event.get("parts")) {
        let original_text = msg.text();
        let mut list = vec![msg.clone()];
        crate::plugin::apply_messages_hook(&mut list, &serde_json::json!([{ "info": info, "parts": parts }]));
        let mut next = list.remove(0);
        // A prompt hook that only adds files/parts must not erase the message
        // text the user actually typed.
        if next.text().trim().is_empty() && !original_text.trim().is_empty() {
            let mut parts = vec![Part::Text { text: original_text, synthetic: false }];
            parts.extend(next.parts.into_iter().filter(|part| matches!(part, Part::File(_))));
            next.parts = parts;
        }
        *msg = next;
    }
}

async fn turn(app: Arc<App>, handle: Arc<Handle>, mut msg: Message, opts: PromptOptions) -> Result<bool> {
    let agent_id = opts.agent.clone().or_else(|| handle.with(|s| s.agent.clone())).unwrap_or_else(|| app.default_agent());
    let agent = app.agent(&agent_id)?.clone();
    let session_model = handle.with(|s| s.model.clone());
    let mut sel = select_model(&app, &agent, &opts, session_model.as_deref())?;
    // V2 `experimental.model.resolve`: a router plugin can serve this request
    // with a different model without changing the session's selection.
    resolve_model_hook(&app, &agent, &mut sel, &handle.id(), "primary").await;
    let full = sel.model.full_id();
    let model_ref = match &sel.variant {
        Some(v) => format!("{full}#{v}"),
        None => full.clone(),
    };
    msg.agent = Some(agent.id.clone());
    msg.model = Some(model_ref.clone());
    if let Some(text) = plan_reminder(&agent.id, &handle.with(|s| s.messages.clone())) {
        msg.parts.insert(0, Part::Text { text, synthetic: true });
    }
    let user_message_id = msg.id.clone();
    let previous = handle.with(|s| (s.agent.clone(), s.model.clone()));
    let agent_changed = previous.0.as_deref() != Some(agent.id.as_str());
    let model_changed = previous.1.as_deref() != Some(full.as_str());
    let first = handle.with(|s| {
        s.agent = Some(agent.id.clone());
        s.model = Some(full.clone());
        if agent_changed {
            s.record(serde_json::json!({
                "type": "agent-switched",
                "agent": agent.id,
                "previous": previous.0.clone(),
            }));
        }
        if model_changed {
            s.record(serde_json::json!({
                "type": "model-switched",
                "model": { "providerID": sel.provider.id, "id": sel.model.id, "variant": sel.variant },
                "previous": previous.1.as_deref().and_then(crate::models::parse_ref).map(|(provider_id, id, variant)| {
                    serde_json::json!({ "providerID": provider_id, "id": id, "variant": variant })
                }),
            }));
        }
        s.messages.push(msg);
        s.messages.iter().filter(|m| m.role == Role::User).count() == 1 && s.has_default_title()
    });
    handle.save();
    if agent_changed {
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.agent.selected",
            "properties": { "sessionID": handle.id(), "agent": agent.id.clone(), "previous": previous.0 }
        }));
    }
    if model_changed {
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.model.selected",
            "properties": {
                "sessionID": handle.id(),
                "model": model_hook_json(&sel),
                "previous": previous.1.as_deref().and_then(crate::models::parse_ref).map(|(provider_id, id, variant)| {
                    serde_json::json!({ "providerID": provider_id, "id": id, "variant": variant })
                })
            }
        }));
    }
    if handle.with(|s| s.parent_id.is_none()) {
        crate::state::push_recent_model(&model_ref);
    }
    emit_message(&app, &handle, &handle.id(), &user_message_id);
    app.emit(AppEvent::Updated { session: handle.id() });
    if first {
        let (a, h) = (app.clone(), handle.clone());
        tokio::spawn(async move {
            // best effort: the session keeps its default title on failure
            let _ = generate_title(&a, &h).await;
            let title = h.with(|s| s.title.clone());
            a.emit(AppEvent::Title { session: h.id(), title });
        });
    }
    run_loop(app, handle, agent, sel).await
}

/// Publish a `message.updated` event for plugin subscribers.
fn emit_message(app: &App, handle: &Handle, session_id: &str, message_id: &str) {
    let message = handle.with(|s| s.messages.iter().find(|m| m.id == message_id).cloned());
    if let Some(message) = message {
        if let Some(item) = crate::plugin::messages_for_hook(session_id, &[message])
            .as_array()
            .and_then(|items| items.first())
            .cloned()
        {
            app.plugins.emit(serde_json::json!({
                "id": crate::util::id("evt"),
                "type": "message.updated",
                "properties": {
                    "sessionID": session_id,
                    "info": item.get("info").cloned().unwrap_or(serde_json::Value::Null),
                    "parts": item.get("parts").cloned().unwrap_or(serde_json::json!([])),
                }
            }));
        }
    }
}

/// Stamp the end time of the open thought when other output starts.
fn close_reasoning(msg: &mut Message) {
    if let Some(Part::Reasoning { started: Some(_), ended: ended @ None, .. }) = msg.parts.iter_mut().rev().find(|p| matches!(p, Part::Reasoning { .. })) {
        *ended = Some(crate::util::now_ms());
    }
}

fn apply_event(msg: &mut Message, ev: &Event, reasoning_open: &mut bool) {
    if matches!(ev, Event::TextDelta(_) | Event::ToolInputStart { .. } | Event::ToolCall { .. } | Event::Finish(_)) {
        close_reasoning(msg);
    }
    match ev {
        Event::TextDelta(t) => match msg.parts.last_mut() {
            Some(Part::Text { text, synthetic: false }) => text.push_str(t),
            _ => msg.parts.push(Part::Text { text: t.clone(), synthetic: false }),
        },
        Event::ReasoningDelta(t) => {
            if *reasoning_open {
                if let Some(Part::Reasoning { text, .. }) = msg.parts.iter_mut().rev().find(|p| matches!(p, Part::Reasoning { .. })) {
                    text.push_str(t);
                    return;
                }
            }
            *reasoning_open = true;
            let now = crate::util::now_ms();
            msg.parts.push(Part::Reasoning { text: t.clone(), signature: None, redacted: None, item_id: None, started: Some(now), ended: None });
        }
        Event::ReasoningEnd { signature: sig, redacted: red, item_id: iid } => {
            if *reasoning_open {
                *reasoning_open = false;
                if let Some(Part::Reasoning { signature, redacted, item_id, ended, .. }) = msg.parts.iter_mut().rev().find(|p| matches!(p, Part::Reasoning { .. })) {
                    *signature = sig.clone();
                    *redacted = red.clone();
                    *item_id = iid.clone();
                    ended.get_or_insert_with(crate::util::now_ms);
                }
            } else if sig.is_some() || red.is_some() {
                msg.parts.push(Part::Reasoning { text: String::new(), signature: sig.clone(), redacted: red.clone(), item_id: iid.clone(), started: None, ended: None });
            }
        }
        Event::ToolInputStart { id, name } => {
            if !id.is_empty() && !msg.parts.iter().any(|p| matches!(p, Part::Tool { id: pid, .. } if pid == id)) {
                msg.parts.push(Part::Tool { id: id.clone(), name: name.clone(), input: Value::Object(Default::default()), state: ToolState::Pending, provider_meta: None });
            }
        }
        Event::ToolCall { id, name, input, meta } => {
            let existing = msg.parts.iter_mut().find(|p| matches!(p, Part::Tool { id: pid, .. } if pid == id));
            match existing {
                Some(Part::Tool { input: i, provider_meta, name: n, .. }) => {
                    *i = input.clone();
                    *provider_meta = meta.clone();
                    *n = name.clone();
                }
                _ => msg.parts.push(Part::Tool { id: id.clone(), name: name.clone(), input: input.clone(), state: ToolState::Pending, provider_meta: meta.clone() }),
            }
        }
        Event::Usage(u) => msg.usage = u.clone(),
        Event::Finish(f) => msg.finish = Some(f.clone()),
        Event::ToolInputDelta { .. } => {}
        Event::Retry { .. } => {}
    }
}

struct StepResult {
    finish: String,
    usage: Usage,
    tool_calls: usize,
    rejected: bool,
    aborted: bool,
    message_id: String,
}

async fn run_loop(app: Arc<App>, handle: Arc<Handle>, agent: Agent, sel: Selection) -> Result<bool> {
    let sid = handle.id();
    let registry = tools::registry(&app, &agent, &sel.model).await;
    let max_steps = agent.steps;
    let mut step = 0usize;
    let mut length_retries = 0;
    let mut interrupted = false;
    loop {
        // steering: fold queued user messages into the conversation, leaving
        // `queue` deliveries for the next turn
        let drained: Vec<(Message, PromptOptions)> = handle.queue.lock().unwrap().drain(..).collect();
        let mut queued = Vec::new();
        let mut deferred = Vec::new();
        for item in drained {
            if item.1.delivery.as_deref() == Some("queue") {
                deferred.push(item);
            } else {
                queued.push(item);
            }
        }
        if !deferred.is_empty() {
            let mut queue = handle.queue.lock().unwrap();
            for item in deferred.into_iter().rev() {
                queue.push_front(item);
            }
        }
        if !queued.is_empty() {
            for (message, _) in &queued {
                app.plugins.emit(serde_json::json!({
                    "id": crate::util::id("evt"),
                    "type": "session.inbox.delivered",
                    "properties": { "sessionID": sid, "inboxID": message.id },
                }));
            }
            handle.with(|s| s.messages.extend(queued.into_iter().map(|(m, _)| m)));
            handle.save();
        }
        step += 1;
        let last = max_steps.is_some_and(|m| step >= m);
        let active_tools: Vec<Arc<dyn Tool>> = if last { vec![] } else { registry.clone() };
        let res = match step_once(&app, &handle, &agent, &sel, &active_tools, &registry, last).await {
            Ok(result) => result,
            Err(error) => {
                let message_id = handle.with(|session| {
                    session
                        .messages
                        .iter()
                        .rev()
                        .find(|message| message.role == crate::session::Role::Assistant)
                        .map(|message| message.id.clone())
                });
                app.plugins.emit(serde_json::json!({
                    "id": crate::util::id("evt"),
                    "type": "session.step.failed",
                    "properties": {
                        "sessionID": sid.clone(),
                        "assistantMessageID": message_id,
                        "error": { "type": "unknown", "message": error.to_string() },
                    },
                }));
                return Err(error);
            }
        };
        app.emit(AppEvent::StepFinished { session: sid.clone() });
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.step.ended",
            "properties": {
                "sessionID": sid.clone(),
                "assistantMessageID": res.message_id.clone(),
                "finish": res.finish.clone(),
                "cost": cost(&sel.model, &res.usage),
                "tokens": {
                    "input": res.usage.input,
                    "output": res.usage.output,
                    "reasoning": res.usage.reasoning,
                    "cache": { "read": res.usage.cache_read, "write": res.usage.cache_write },
                },
            },
        }));
        let (total_usage, total_cost) = handle.with(|session| session.usage_total());
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.usage.updated",
            "properties": {
                "sessionID": sid.clone(),
                "cost": total_cost,
                "tokens": {
                    "input": total_usage.input,
                    "output": total_usage.output,
                    "reasoning": total_usage.reasoning,
                    "cache": { "read": total_usage.cache_read, "write": total_usage.cache_write },
                },
            },
        }));
        if res.aborted || res.rejected {
            interrupted = true;
            break;
        }
        // automatic compaction near the context limit (skipped right after a
        // compaction, when there is nothing new to summarize)
        let fresh_checkpoint = handle.with(|s| {
            let active = s.active_messages();
            active.first().is_some_and(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. }))) && active.len() <= 3
        });
        if app.config().compaction.auto
            && !fresh_checkpoint
            && sel.model.context > 0
            && res.usage.total_context() as f64
                >= sel.model.context as f64 * app.config().compaction.threshold(sel.model.context)
        {
            let continuing = res.tool_calls > 0;
            if let Err(e) = compact(&app, &handle, &agent, &sel, &registry, continuing).await {
                app.emit(AppEvent::Error { session: sid.clone(), message: format!("compaction failed: {e}") });
                break;
            }
            if !continuing {
                break;
            }
            continue;
        }
        let has_queue = handle
            .queue
            .lock()
            .unwrap()
            .iter()
            .any(|(_, options)| options.delivery.as_deref() != Some("queue"));
        match res.finish.as_str() {
            "tool-calls" if res.tool_calls > 0 && !last => continue,
            "pause" => continue,
            "length" if res.tool_calls == 0 && length_retries < 2 => {
                length_retries += 1;
                let mut m = Message::new(Role::User);
                m.parts.push(Part::Text { text: crate::prompt::LENGTH.into(), synthetic: true });
                handle.with(|s| s.messages.push(m));
                continue;
            }
            _ if has_queue => continue,
            _ => break,
        }
    }
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(interrupted)
}

pub(crate) async fn apply_aisdk_hooks(app: &Arc<App>, req: &mut Request, sel: &Selection) {
    let model = model_hook_json(sel);
    let sdk = app
        .plugins
        .trigger(
            "aisdk",
            "sdk",
            serde_json::json!({
                "model": model,
                "package": req.provider.npm,
                "options": req.options,
            }),
        )
        .await;
    if let Some(options) = sdk.get("options").filter(|value| value.is_object()) {
        req.options = options.clone();
    }
    let sdk_value = sdk.get("sdk").cloned().unwrap_or(Value::Null);
    let language = app
        .plugins
        .trigger(
            "aisdk",
            "language",
            serde_json::json!({
                "model": model,
                "sdk": sdk_value,
                "options": req.options,
            }),
        )
        .await;
    if let Some(options) = language.get("options").filter(|value| value.is_object()) {
        req.options = options.clone();
    }
}

/// Let a V2 router plugin replace the model for a session request kind.
pub(crate) async fn resolve_model_hook(
    app: &Arc<App>,
    agent: &Agent,
    sel: &mut Selection,
    session_id: &str,
    kind: &str,
) {
    let resolved = app
        .plugins
        .trigger(
            "session",
            "experimental.model.resolve",
            serde_json::json!({
                "sessionID": session_id,
                "agent": agent.id,
                "kind": kind,
                "model": {
                    "providerID": sel.provider.id,
                    "id": sel.model.id,
                    "variant": sel.variant,
                },
            }),
        )
        .await;
    let Some(model) = resolved.get("model") else { return };
    if let (Some(provider_id), Some(model_id)) = (
        model.get("providerID").and_then(Value::as_str),
        model.get("id").and_then(Value::as_str),
    ) {
        let next_variant = model.get("variant").and_then(Value::as_str);
        if provider_id == sel.provider.id && model_id == sel.model.id && next_variant == sel.variant.as_deref() {
            return;
        }
        let mut reference = format!("{provider_id}/{model_id}");
        if let Some(variant) = next_variant {
            reference.push('#');
            reference.push_str(variant);
        }
        if let Ok(next) = select_model(
            app,
            agent,
            &PromptOptions { model: Some(reference), ..Default::default() },
            Some(&sel.model.full_id()),
        ) {
            *sel = next;
        }
    }
}

pub(crate) fn attach_hooks(app: &Arc<App>, req: &mut Request, session_id: &str, agent: &Agent, sel: &Selection, kind: &str) {
    req.hooks = Some(provider::HookContext {
        app: app.clone(),
        session_id: session_id.to_string(),
        agent: agent.id.clone(),
        model: model_hook_json(sel),
        kind: kind.to_string(),
    });
}

pub(crate) fn model_hook_json(sel: &Selection) -> Value {
    serde_json::json!({
        "id": sel.model.id,
        "modelID": sel.model.id,
        "providerID": sel.provider.id,
        "variant": sel.variant,
        "name": sel.model.name,
        "family": sel.model.family,
        "capabilities": {
            "tools": sel.model.tool_call,
            "input": sel.model.input_modalities,
            "output": ["text"],
            "temperature": sel.model.temperature,
        },
        "cost": [{
            "input": sel.model.cost_input,
            "output": sel.model.cost_output,
            "cache": { "read": sel.model.cost_cache_read, "write": sel.model.cost_cache_write },
        }],
        "limit": { "context": sel.model.context, "output": sel.model.output },
        "variants": [],
        "status": sel.model.status.clone().unwrap_or_else(|| "active".into()),
        "enabled": true,
    })
}

pub(crate) fn build_request(
    sel: &Selection,
    agent: &Agent,
    system: Vec<String>,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    session: &provider::SessionRef,
) -> Request {
    Request {
        provider: sel.provider.clone(),
        model: sel.model.clone(),
        variant: sel.variant.clone(),
        system,
        messages,
        tools,
        temperature: agent.temperature,
        top_p: agent.top_p,
        max_tokens: provider::max_output(&sel.model),
        options: Value::Null,
        headers: Vec::new(),
        session: session.clone(),
        hooks: None,
    }
}

/// One model call plus execution of the tool calls it made.
async fn step_once(
    app: &Arc<App>,
    handle: &Arc<Handle>,
    agent: &Agent,
    sel: &Selection,
    active: &[Arc<dyn Tool>],
    registry: &[Arc<dyn Tool>],
    last_step: bool,
) -> Result<StepResult> {
    let sid = handle.id();
    let names: Vec<String> = active.iter().map(|t| t.name().to_string()).collect();
    let directory = handle.with(|session| session.directory.clone());
    let mut system = crate::prompt::system_in(app, agent, &sel.model, &names, &directory).await;
    let entries: Vec<String> = handle.with(|session| {
        session
            .instruction_entries
            .values()
            .filter_map(|value| {
                value
                    .as_str()
                    .map(String::from)
                    .or_else(|| value.get("text").and_then(Value::as_str).map(String::from))
            })
            .collect()
    });
    system.extend(entries);
    let mut messages: Vec<Message> = handle.with(|s| s.active_messages().to_vec());
    if last_step {
        let mut m = Message::new(Role::User);
        m.parts.push(Part::Text { text: crate::prompt::MAX_STEPS.into(), synthetic: true });
        messages.push(m);
    }
    let model_json = model_hook_json(sel);

    // V1 system transform. V2 plugins get the same data through session.context.
    let transformed = app
        .plugins
        .trigger(
            "session",
            "system.transform",
            serde_json::json!({ "sessionID": sid, "model": model_json, "system": system }),
        )
        .await;
    if let Some(list) = transformed.get("system").and_then(Value::as_array) {
        system = list.iter().filter_map(Value::as_str).map(String::from).collect();
    }

    // V1 experimental.chat.messages.transform.
    let transformed = app
        .plugins
        .trigger(
            "session",
            "messages.transform",
            serde_json::json!({ "messages": crate::plugin::messages_for_hook(&sid, &messages) }),
        )
        .await;
    if let Some(list) = transformed.get("messages") {
        crate::plugin::apply_messages_hook(&mut messages, list);
    }

    // tools must stay declared when history contains tool calls (provider requirement)
    let mut specs: Vec<ToolSpec> = if active.is_empty() && messages.iter().any(|m| m.tool_calls().next().is_some()) && !last_step {
        registry.iter().map(|t| tools::spec(t.as_ref())).collect()
    } else {
        active.iter().map(|t| tools::spec(t.as_ref())).collect()
    };
    for spec in specs.iter_mut() {
        let transformed = app
            .plugins
            .trigger(
                "tool",
                "definition",
                serde_json::json!({
                    "toolID": spec.name,
                    "description": spec.description,
                    "parameters": spec.schema,
                }),
            )
            .await;
        if let Some(description) = transformed.get("description").and_then(Value::as_str) {
            spec.description = description.to_string();
        }
        if let Some(parameters) = transformed.get("parameters").filter(|v| v.is_object()) {
            spec.schema = parameters.clone();
        }
    }

    // V2 session.context + V1 chat.params/chat.headers.
    let tool_map: serde_json::Map<String, Value> = specs
        .iter()
        .map(|spec| (spec.name.clone(), serde_json::json!({ "description": spec.description, "input": spec.schema })))
        .collect();
    let last_user = crate::plugin::messages_for_hook(&sid, &messages)
        .as_array()
        .and_then(|a| a.iter().rev().find(|m| m.pointer("/info/role").and_then(Value::as_str) == Some("user")).cloned())
        .unwrap_or(Value::Null);
    let context_event = app
        .plugins
        .trigger(
            "session",
            "context",
            serde_json::json!({
                "sessionID": sid,
                "agent": agent.id,
                "model": model_json,
                "provider": {
                    "source": "config",
                    "info": { "id": sel.provider.id, "name": sel.provider.name },
                    "options": sel.provider.options,
                },
                "message": last_user,
                // V2 SystemPart[]; V1 chat.params ignores this field.
                "system": system.iter().map(|text| serde_json::json!({ "type": "text", "text": text })).collect::<Vec<_>>(),
                "messages": crate::plugin::messages_for_hook(&sid, &messages),
                "options": {
                    "temperature": agent.temperature,
                    "topP": agent.top_p,
                    "topK": Value::Null,
                    "maxTokens": provider::max_output(&sel.model),
                    "maxOutputTokens": provider::max_output(&sel.model),
                    "options": {},
                },
                "tools": tool_map,
                "headers": {},
            }),
        )
        .await;
    if let Some(list) = context_event.get("system").and_then(Value::as_array) {
        system = list
            .iter()
            .filter_map(|item| item.as_str().map(String::from).or_else(|| item.get("text").and_then(Value::as_str).map(String::from)))
            .collect();
    }
    if let Some(list) = context_event.get("messages") {
        crate::plugin::apply_messages_hook(&mut messages, list);
    }
    if let Some(tools) = context_event.get("tools").and_then(Value::as_object) {
        specs.retain(|spec| tools.contains_key(&spec.name));
        for spec in specs.iter_mut() {
            if let Some(tool) = tools.get(&spec.name) {
                if let Some(description) = tool.get("description").and_then(Value::as_str) {
                    spec.description = description.to_string();
                }
                if let Some(input) = tool.get("input").filter(|v| v.is_object()) {
                    spec.schema = input.clone();
                }
            }
        }
    }
    let options = context_event.get("options").cloned().unwrap_or(Value::Null);
    let headers_event = app
        .plugins
        .trigger(
            "session",
            "headers",
            serde_json::json!({
                "sessionID": sid,
                "agent": agent.id,
                "model": model_json,
                "provider": {
                    "source": "config",
                    "info": { "id": sel.provider.id, "name": sel.provider.name },
                    "options": sel.provider.options,
                },
                "message": last_user,
                "headers": context_event.get("headers").cloned().unwrap_or_else(|| serde_json::json!({})),
            }),
        )
        .await;
    let headers: Vec<(String, String)> = headers_event
        .get("headers")
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default();
    let mut req = build_request(sel, agent, system, messages, specs, &handle.with(|s| provider::SessionRef::of(s)));
    attach_hooks(app, &mut req, &sid, agent, sel, "primary");
    if let Some(temperature) = options.get("temperature").and_then(Value::as_f64) {
        req.temperature = Some(temperature);
    }
    if let Some(top_p) = options.get("topP").and_then(Value::as_f64) {
        req.top_p = Some(top_p);
    }
    if let Some(max) = options
        .get("maxTokens")
        .and_then(Value::as_u64)
        .or_else(|| options.get("maxOutputTokens").and_then(Value::as_u64))
    {
        req.max_tokens = max;
    }
    if let Some(extra) = options.get("options").filter(|v| v.is_object()) {
        req.options = extra.clone();
    } else if options.is_object() {
        // V2 SessionRequestOptions is flat: unknown keys are provider options.
        let mut extra = serde_json::Map::new();
        for (key, value) in options.as_object().unwrap() {
            if !matches!(key.as_str(), "temperature" | "topP" | "topK" | "maxTokens" | "maxOutputTokens" | "options") {
                extra.insert(key.clone(), value.clone());
            }
        }
        req.options = Value::Object(extra);
    }
    req.headers = headers;
    apply_aisdk_hooks(app, &mut req, sel).await;
    let model_request = app
        .plugins
        .trigger(
            "session",
            "model.request",
            serde_json::json!({
                "sessionID": sid,
                "agent": agent.id,
                "model": model_json,
                "kind": "primary",
                "baseURL": sel.provider.base_url(&sel.model),
                "headers": req.headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<serde_json::Map<String, Value>>(),
            }),
        )
        .await;
    if let Some(headers) = model_request.get("headers").and_then(Value::as_object) {
        req.headers = headers.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
    }
    if let Some(base) = model_request.get("baseURL").and_then(Value::as_str) {
        if base != sel.provider.base_url(&sel.model).unwrap_or_default() {
            req.provider.options.base_url = Some(base.to_string());
        }
    }

    let mut assistant = Message::new(Role::Assistant);
    assistant.agent = Some(agent.id.clone());
    assistant.model = Some(sel.model.full_id());
    let msg_id = assistant.id.clone();
    handle.with(|s| s.messages.push(assistant));
    emit_message(app, handle, &sid, &msg_id);
    app.emit(AppEvent::Updated { session: sid.clone() });
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.step.started",
        "properties": {
            "sessionID": sid.clone(),
            "assistantMessageID": msg_id.clone(),
            "agent": agent.id.clone(),
            "model": model_hook_json(sel),
            "started": crate::util::now_ms(),
        },
    }));

    let cancel = CancellationToken::new();
    *handle.cancel.lock().unwrap() = Some(cancel.clone());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(provider::stream(req, tx));
    let mut reasoning_open = false;
    let mut aborted = false;
    let mut streamed = false;
    let update = |f: &mut dyn FnMut(&mut Message)| {
        handle.with(|s| {
            if let Some(m) = s.messages.iter_mut().rev().find(|m| m.id == msg_id) {
                f(m);
            }
        })
    };
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                if !streamed {
                    streamed = true;
                    app.plugins.emit(serde_json::json!({
                        "id": crate::util::id("evt"),
                        "type": "session.step.streamed",
                        "properties": { "sessionID": sid.clone(), "assistantMessageID": msg_id.clone() },
                    }));
                }
                match &ev {
                    Event::TextDelta(d) => app.emit(AppEvent::TextDelta { session: sid.clone(), message_id: msg_id.clone(), delta: d.clone() }),
                    Event::ReasoningDelta(d) => app.emit(AppEvent::ReasoningDelta { session: sid.clone(), message_id: msg_id.clone(), delta: d.clone() }),
                    Event::ToolInputStart { id, name } => app.plugins.emit(serde_json::json!({
                        "id": crate::util::id("evt"),
                        "type": "session.tool.input.started",
                        "properties": { "sessionID": sid.clone(), "assistantMessageID": msg_id.clone(), "id": id.clone(), "name": name.clone() },
                    })),
                    Event::ToolInputDelta { id, delta } => app.plugins.emit(serde_json::json!({
                        "id": crate::util::id("evt"),
                        "type": "session.tool.input.delta",
                        "properties": {
                            "sessionID": sid.clone(),
                            "assistantMessageID": msg_id.clone(),
                            "id": id.clone(),
                            "delta": delta.clone(),
                        },
                    })),
                    Event::ToolCall { id, name, input, .. } => app.plugins.emit(serde_json::json!({
                        "id": crate::util::id("evt"),
                        "type": "session.tool.input.ended",
                        "properties": {
                            "sessionID": sid.clone(),
                            "assistantMessageID": msg_id.clone(),
                            "id": id.clone(),
                            "name": name.clone(),
                            "text": input.to_string(),
                        },
                    })),
                    Event::Retry { message, attempt, delay } => app.emit(AppEvent::Retry {
                        session: sid.clone(),
                        message: message.clone(),
                        assistant_message_id: msg_id.clone(),
                        attempt: *attempt,
                        at: crate::util::now_ms() + *delay as i64,
                    }),
                    _ => app.emit(AppEvent::Updated { session: sid.clone() }),
                }
                update(&mut |m| apply_event(m, &ev, &mut reasoning_open));
            }
            _ = cancel.cancelled() => {
                task.abort();
                aborted = true;
                break;
            }
        }
    }
    let stream_result = if aborted { Ok(Ok(())) } else { task.await };
    let error = match stream_result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) if e.is_cancelled() => None,
        Err(e) => Some(format!("provider task failed: {e}")),
    };

    let model = sel.model.clone();
    let done = handle.with(|s| {
        // undo can remove the message while the step is still winding down
        let m = s.messages.iter_mut().rev().find(|m| m.id == msg_id)?;
        m.cost = cost(&model, &m.usage);
        m.completed = Some(crate::util::now_ms());
        if aborted {
            m.error = Some("The operation was aborted.".into());
            m.finish = Some("aborted".into());
        }
        if let Some(e) = &error {
            m.error = Some(e.clone());
            m.finish = Some("error".into());
        }
        Some((m.usage.clone(), m.finish.clone().unwrap_or_else(|| "unknown".into())))
    });
    let Some((usage, finish)) = done else {
        return Ok(StepResult {
            finish: "aborted".into(),
            usage: Usage::default(),
            tool_calls: 0,
            rejected: false,
            aborted: true,
            message_id: msg_id,
        });
    };
    emit_message(app, handle, &sid, &msg_id);
    if let Some(e) = &error {
        app.emit(AppEvent::Error { session: sid.clone(), message: e.clone() });
    }
    if finish == "refusal" {
        app.emit(AppEvent::Error { session: sid.clone(), message: "The model declined to continue (refusal). Try rephrasing or switching models.".into() });
    }
    // V1 experimental.text.complete / V2 text end: let plugins rewrite text.
    let text_updates = handle.with(|s| {
        s.messages
            .iter()
            .rev()
            .find(|m| m.id == msg_id)
            .map(|message| {
                message
                    .parts
                    .iter()
                    .enumerate()
                    .filter_map(|(index, part)| match part {
                        Part::Text { text, .. } => Some((index, text.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    for (index, text) in text_updates {
        let event = app
            .plugins
            .trigger(
                "session",
                "text.complete",
                serde_json::json!({
                    "sessionID": sid,
                    "messageID": msg_id,
                    "partID": format!("{msg_id}:{index}"),
                    "text": text,
                }),
            )
            .await;
        if let Some(next) = event.get("text").and_then(Value::as_str) {
            handle.with(|s| {
                if let Some(message) = s.messages.iter_mut().rev().find(|m| m.id == msg_id) {
                    if let Some(Part::Text { text, .. }) = message.parts.get_mut(index) {
                        *text = next.to_string();
                    }
                }
            });
        }
    }

    // execute tool calls
    let calls: Vec<(String, String, Value)> = handle.with(|s| {
        s.messages
            .iter()
            .rev()
            .find(|m| m.id == msg_id)
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Tool { id, name, input, state: ToolState::Pending, .. } => Some((id.clone(), name.clone(), input.clone())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    let set_state = |call_id: &str, state: ToolState| {
        handle.with(|s| {
            if let Some(m) = s.messages.iter_mut().rev().find(|m| m.id == msg_id) {
                for p in m.parts.iter_mut() {
                    if let Part::Tool { id, state: st, .. } = p {
                        if id == call_id {
                            *st = state.clone();
                        }
                    }
                }
            }
        })
    };
    if aborted || error.is_some() {
        for (id, _, _) in &calls {
            set_state(id, ToolState::Error { error: "Tool execution aborted".into() });
        }
        handle.save();
        return Ok(StepResult {
            finish,
            usage,
            tool_calls: 0,
            rejected: false,
            aborted: true,
            message_id: msg_id,
        });
    }

    let n_calls = calls.len();
    let rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let futs = calls.into_iter().map(|(call_id, name, input)| {
        let app = app.clone();
        let handle = handle.clone();
        let agent = agent.clone();
        let model = sel.model.clone();
        let sid = sid.clone();
        let cancel = cancel.clone();
        let rejected = rejected.clone();
        let tool = registry.iter().find(|t| t.name() == name).cloned();
        let available: Vec<String> = active.iter().map(|t| t.name().to_string()).collect();
        let set_state = &set_state;
        let message_id = msg_id.clone();
        async move {
            let mut input = input;
            set_state(&call_id, ToolState::Running);
            app.emit(AppEvent::ToolStarted { session: sid.clone(), message_id: message_id.clone(), id: call_id.clone(), name: name.clone(), input: input.clone() });
            let before = app
                .plugins
                .trigger(
                    "tool",
                    "execute.before",
                    serde_json::json!({
                        "tool": name,
                        "sessionID": sid,
                        "agent": agent.id,
                        "messageID": message_id,
                        "id": call_id,
                        "input": input,
                    }),
                )
                .await;
            if let Some(error) = before.get("__error").and_then(Value::as_str) {
                let state = ToolState::Error { error: error.to_string() };
                app.emit(AppEvent::ToolFinished {
                    session: sid.clone(),
                    message_id: message_id.clone(),
                    id: call_id.clone(),
                    name: name.clone(),
                    title: String::new(),
                    output: String::new(),
                    error: Some(error.to_string()),
                    metadata: serde_json::Value::Null,
                });
                set_state(&call_id, state);
                return;
            }
            if let Some(next) = before.get("input") {
                input = next.clone();
            }
            let result: Result<tools::Output> = match tool {
                _ if input.get("__invalid_json").is_some() => Err(anyhow!(
                    "Invalid JSON in tool arguments ({}). Retry the call with valid JSON arguments.",
                    input.get("__error").and_then(|e| e.as_str()).unwrap_or("parse error")
                )),
                None => Err(anyhow!("Unknown tool: {name}. Available tools: {}", available.join(", "))),
                Some(_) if !available.contains(&name) => Err(anyhow!("Tool {name} is not available right now.")),
                Some(t) => {
                    let ctx = tools::Context {
                        app: app.clone(),
                        handle: handle.clone(),
                        agent: agent.clone(),
                        model: model.clone(),
                        session_id: sid.clone(),
                        message_id: message_id.clone(),
                        call_id: call_id.clone(),
                        cancel: cancel.clone(),
                    };
                    tokio::select! {
                        r = t.execute(input.clone(), &ctx) => r,
                        _ = cancel.cancelled() => {
                            app.plugins.abort_tool(&call_id);
                            Err(anyhow!("Tool execution aborted"))
                        }
                    }
                }
            };
            let state = match result {
                Ok(mut out) => {
                    if let Some(progress) = app.take_tool_progress(&call_id) {
                        if let (Some(target), Some(source)) = (out.metadata.as_object_mut(), progress.as_object()) {
                            for (key, value) in source {
                                target.insert(key.clone(), value.clone());
                            }
                        } else {
                            out.metadata = progress;
                        }
                    }
                    let after = app
                        .plugins
                        .trigger(
                            "tool",
                            "execute.after",
                            serde_json::json!({
                                "tool": name,
                                "sessionID": sid,
                                "agent": agent.id,
                                "messageID": message_id,
                                "id": call_id,
                                "input": input,
                                "status": "completed",
                                "result": {
                                    "output": out.output,
                                    "content": out.output,
                                    "title": out.title,
                                    "metadata": out.metadata,
                                },
                            }),
                        )
                        .await;
                    if let Some(result) = after.get("result") {
                        if let Some(output) = result
                            .get("output")
                            .and_then(Value::as_str)
                            .or_else(|| result.get("content").and_then(Value::as_str))
                        {
                            out.output = output.to_string();
                        }
                        if let Some(title) = result.get("title").and_then(Value::as_str) {
                            out.title = title.to_string();
                        }
                        if let Some(metadata) = result.get("metadata").filter(|value| !value.is_null()) {
                            out.metadata = metadata.clone();
                        }
                    }
                    let mut text = if matches!(name.as_str(), "read" | "shell") { out.output } else { tools::truncate_output(&out.output).0 };
                    if let Some(c) = out.context {
                        text.push_str("\n\n");
                        text.push_str(&c);
                    }
                    app.emit(AppEvent::ToolFinished { session: sid.clone(), message_id: message_id.clone(), id: call_id.clone(), name: name.clone(), title: out.title.clone(), output: text.clone(), error: None, metadata: out.metadata.clone() });
                    ToolState::Completed { output: text, title: out.title, metadata: out.metadata, attachments: out.attachments }
                }
                Err(e) => {
                    app.take_tool_progress(&call_id);
                    let msg = e.to_string();
                    if msg == REJECTED {
                        rejected.store(true, Ordering::SeqCst);
                    }
                    app.plugins
                        .trigger(
                            "tool",
                            "execute.after",
                            serde_json::json!({
                                "tool": name,
                                "sessionID": sid,
                                "agent": agent.id,
                                "messageID": message_id,
                                "id": call_id,
                                "input": input,
                                "status": "error",
                                "error": { "_tag": "Tool.Error", "message": msg },
                            }),
                        )
                        .await;
                    app.emit(AppEvent::ToolFinished { session: sid.clone(), message_id: message_id.clone(), id: call_id.clone(), name: name.clone(), title: String::new(), output: String::new(), error: Some(msg.clone()), metadata: serde_json::Value::Null });
                    ToolState::Error { error: msg }
                }
            };
            set_state(&call_id, state);
        }
    });
    futures::future::join_all(futs).await;
    *handle.cancel.lock().unwrap() = None;
    let was_cancelled = cancel.is_cancelled();
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(StepResult {
        finish,
        usage,
        tool_calls: n_calls,
        rejected: rejected.load(Ordering::SeqCst),
        aborted: was_cancelled,
        message_id: msg_id,
    })
}

/// Run one request to completion and collect its text.
pub(crate) async fn complete_request(req: Request, cancel: Option<CancellationToken>) -> Result<(String, Usage)> {
    complete_request_with(req, cancel, |_| {}).await
}

pub(crate) async fn complete_request_with(
    req: Request,
    cancel: Option<CancellationToken>,
    mut on_delta: impl FnMut(&str),
) -> Result<(String, Usage)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(provider::stream(req, tx));
    let mut text = String::new();
    let mut usage = Usage::default();
    let cancel = cancel.unwrap_or_default();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(Event::TextDelta(t)) => {
                    text.push_str(&t);
                    on_delta(&t);
                }
                Some(Event::Usage(u)) => usage = u,
                Some(_) => {}
                None => break,
            },
            _ = cancel.cancelled() => { task.abort(); return Err(anyhow!("aborted")); }
        }
    }
    task.await.map_err(|e| anyhow!("{e}"))??;
    Ok((text, usage))
}

/// Summarize the active history into a compaction checkpoint.
pub async fn compact(app: &Arc<App>, handle: &Arc<Handle>, agent: &Agent, sel: &Selection, registry: &[Arc<dyn Tool>], continuing: bool) -> Result<()> {
    let sid = handle.id();
    let mut routed = sel.clone();
    resolve_model_hook(app, agent, &mut routed, &sid, "compaction").await;
    let sel = &routed;
    let mut messages: Vec<Message> = handle.with(|s| s.active_messages().to_vec());
    if messages.is_empty() {
        return Ok(());
    }
    let existing = messages.first().is_some_and(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. })));
    let reason = if continuing { "auto" } else { "manual" };
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.compaction.started",
        "properties": { "sessionID": sid, "reason": reason },
    }));
    // V1 experimental.session.compacting may replace or extend the prompt.
    let compacting = app
        .plugins
        .trigger(
            "session",
            "compacting",
            serde_json::json!({ "sessionID": sid, "context": [], "prompt": Value::Null }),
        )
        .await;
    let mut prompt = compaction_prompt(existing);
    if let Some(custom) = compacting.get("prompt").and_then(Value::as_str) {
        prompt = custom.to_string();
    }
    if let Some(context) = compacting.get("context").and_then(Value::as_array) {
        for item in context.iter().filter_map(Value::as_str) {
            prompt.push_str("\n\n");
            prompt.push_str(item);
        }
    }
    messages.push(Message::user_text(prompt));
    let compaction = app.agent("compaction").unwrap_or_else(|_| agent.clone());
    let names: Vec<String> = registry.iter().map(|t| t.name().to_string()).collect();
    let directory = handle.with(|session| session.directory.clone());
    let mut system = crate::prompt::system_in(app, &compaction, &sel.model, &names, &directory).await;
    let specs = registry.iter().map(|t| tools::spec(t.as_ref())).collect();
    app.emit(AppEvent::Notice { message: "Compacting conversation…".into() });
    let cancel = CancellationToken::new();
    *handle.cancel.lock().unwrap() = Some(cancel.clone());
    // V2 session.compaction can supply the summary directly and skip the model call.
    let compaction_event = app
        .plugins
        .trigger(
            "session",
            "compaction",
            serde_json::json!({
                "sessionID": sid,
                "agent": compaction.id,
                "model": model_hook_json(sel),
                "system": system.iter().map(|text| serde_json::json!({ "type": "text", "text": text })).collect::<Vec<_>>(),
                "messages": crate::plugin::messages_for_hook(&sid, &messages),
                "options": {},
                "tools": {},
                "result": Value::Null,
            }),
        )
        .await;
    let supplied = compaction_event
        .pointer("/result/summary")
        .and_then(Value::as_str)
        .map(String::from);
    let result_metadata = compaction_event.pointer("/result/metadata").cloned().filter(|value| !value.is_null());
    if let Some(list) = compaction_event.get("system").and_then(Value::as_array) {
        system = list
            .iter()
            .filter_map(|item| item.as_str().map(String::from).or_else(|| item.get("text").and_then(Value::as_str).map(String::from)))
            .collect();
    }
    if let Some(list) = compaction_event.get("messages") {
        crate::plugin::apply_messages_hook(&mut messages, list);
    }
    let (summary, usage) = if let Some(summary) = supplied {
        (summary, Usage::default())
    } else {
        let mut req = build_request(sel, &compaction, system, messages, specs, &handle.with(|s| provider::SessionRef::of(s)));
        attach_hooks(app, &mut req, &sid, &compaction, sel, "compaction");
        apply_aisdk_hooks(app, &mut req, sel).await;
        if let Some(options) = compaction_event.get("options") {
            if let Some(temperature) = options.get("temperature").and_then(Value::as_f64) {
                req.temperature = Some(temperature);
            }
            if let Some(top_p) = options.get("topP").and_then(Value::as_f64) {
                req.top_p = Some(top_p);
            }
            if let Some(max) = options
                .get("maxTokens")
                .and_then(Value::as_u64)
                .or_else(|| options.get("maxOutputTokens").and_then(Value::as_u64))
            {
                req.max_tokens = max;
            }
            if let Some(extra) = options.get("options").filter(|value| value.is_object()) {
                req.options = extra.clone();
            }
        }
        let delta_session = sid.clone();
        match complete_request_with(req, Some(cancel), |delta| {
            app.plugins.emit(serde_json::json!({
                "id": crate::util::id("evt"),
                "type": "session.compaction.delta",
                "properties": { "sessionID": delta_session, "text": delta },
            }));
        })
        .await
        {
            Ok(value) => value,
            Err(error) => {
                *handle.cancel.lock().unwrap() = None;
                app.plugins.emit(serde_json::json!({
                    "id": crate::util::id("evt"),
                    "type": "session.compaction.failed",
                    "properties": {
                        "sessionID": sid,
                        "reason": reason,
                        "error": { "type": "unknown", "message": error.to_string() },
                        "cost": 0,
                        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    },
                }));
                return Err(error);
            }
        }
    };
    *handle.cancel.lock().unwrap() = None;
    if summary.trim().is_empty() {
        app.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.compaction.failed",
            "properties": {
                "sessionID": sid,
                "reason": reason,
                "error": { "type": "unknown", "message": "the model returned an empty summary" },
                "cost": cost(&sel.model, &usage),
                "tokens": {
                    "input": usage.input,
                    "output": usage.output,
                    "reasoning": usage.reasoning,
                    "cache": { "read": usage.cache_read, "write": usage.cache_write },
                },
            },
        }));
        return Err(anyhow!("the model returned an empty summary"));
    }
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.compaction.ended",
        "properties": {
            "sessionID": sid,
            "reason": reason,
            "model": model_hook_json(sel),
            "text": summary.trim(),
            "cost": cost(&sel.model, &usage),
            "tokens": {
                "input": usage.input,
                "output": usage.output,
                "reasoning": usage.reasoning,
                "cache": { "read": usage.cache_read, "write": usage.cache_write },
            },
        },
    }));
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.usage.recorded",
        "properties": {
            "sessionID": sid.clone(),
            "source": "compaction",
            "cost": cost(&sel.model, &usage),
            "tokens": {
                "input": usage.input,
                "output": usage.output,
                "reasoning": usage.reasoning,
                "cache": { "read": usage.cache_read, "write": usage.cache_write },
            },
        },
    }));
    let mut m = Message::new(Role::User);
    m.parts.push(Part::Compaction { summary: summary.trim().to_string(), metadata: result_metadata });
    if continuing {
        let autocontinue = app
            .plugins
            .trigger(
                "session",
                "autocontinue",
                serde_json::json!({
                    "sessionID": sid,
                    "agent": agent.id,
                    "model": model_hook_json(sel),
                    "message": {},
                    "overflow": false,
                    "enabled": true,
                }),
            )
            .await;
        if autocontinue.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
            m.parts.push(Part::Text { text: "Continue with the next steps of the work above, if there are any.".into(), synthetic: true });
        }
    }
    m.usage = usage.clone();
    m.cost = cost(&sel.model, &usage);
    handle.with(|s| s.messages.push(m));
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(())
}

/// Manual compaction (e.g. `/compact`).
pub async fn compact_session(app: Arc<App>, session_id: &str) -> Result<()> {
    let handle = app.session(session_id)?;
    if handle.busy.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("session is busy"));
    }
    app.emit(AppEvent::Busy { session: session_id.into(), busy: true });
    let res = async {
        let agent = app.agent(&handle.with(|s| s.agent.clone()).unwrap_or_else(|| app.default_agent()))?.clone();
        let sel = select_model(&app, &agent, &PromptOptions::default(), handle.with(|s| s.model.clone()).as_deref())?;
        let registry = tools::registry(&app, &agent, &sel.model).await;
        compact(&app, &handle, &agent, &sel, &registry, false).await
    }
    .await;
    handle.busy.store(false, Ordering::SeqCst);
    app.emit(AppEvent::Busy { session: session_id.into(), busy: false });
    res
}

pub async fn generate_title(app: &Arc<App>, handle: &Arc<Handle>) -> Result<()> {
    let first = handle.with(|s| s.messages.iter().find(|m| m.role == Role::User).map(|m| m.text())).unwrap_or_default();
    if first.trim().is_empty() {
        return Ok(());
    }
    let agent = app.agent("title")?.clone();
    let main = handle.with(|s| s.model.clone()).ok_or_else(|| anyhow!("no model"))?;
    let provider_id = main.split('/').next().unwrap_or("").to_string();
    let providers = app.providers();
    let small = crate::models::small_model(&app.config(), &providers, &provider_id).unwrap_or(main);
    let sel = select_model(app, &agent, &PromptOptions { model: Some(small), ..Default::default() }, None)?;
    let mut sel = Selection { variant: if sel.model.has_reasoning_type("toggle") { Some("off".into()) } else { Some("low".into()).filter(|v| sel.model.variants().contains(v)) }, ..sel };
    resolve_model_hook(app, &agent, &mut sel, &handle.id(), "title").await;
    let input = format!("Here is the first message of the conversation:\n\n<message>\n{first}\n</message>\n\nReply with only the title.");
    let mut req = build_request(&sel, &agent, vec![crate::agent::TITLE_PROMPT.into()], vec![Message::user_text(input)], vec![], &handle.with(|s| provider::SessionRef::of(s)));
    attach_hooks(app, &mut req, &handle.id(), &agent, &sel, "title");
    apply_aisdk_hooks(app, &mut req, &sel).await;
    req.max_tokens = 0;
    let title_event = app
        .plugins
        .trigger(
            "session",
            "title",
            serde_json::json!({
                "sessionID": handle.id(),
                "agent": agent.id,
                "model": model_hook_json(&sel),
                "system": req.system.iter().map(|text| serde_json::json!({ "type": "text", "text": text })).collect::<Vec<_>>(),
                "messages": crate::plugin::messages_for_hook(&handle.id(), &req.messages),
                "options": {},
                "result": Value::Null,
            }),
        )
        .await;
    let (text, usage) = if let Some(result) = title_event.get("result").and_then(Value::as_str) {
        (result.to_string(), Usage::default())
    } else {
        if let Some(list) = title_event.get("system").and_then(Value::as_array) {
            req.system = list
                .iter()
                .filter_map(|item| item.as_str().map(String::from).or_else(|| item.get("text").and_then(Value::as_str).map(String::from)))
                .collect();
        }
        if let Some(list) = title_event.get("messages") {
            crate::plugin::apply_messages_hook(&mut req.messages, list);
        }
        if let Some(options) = title_event.get("options") {
            if let Some(temperature) = options.get("temperature").and_then(Value::as_f64) {
                req.temperature = Some(temperature);
            }
            if let Some(top_p) = options.get("topP").and_then(Value::as_f64) {
                req.top_p = Some(top_p);
            }
            if let Some(max) = options
                .get("maxTokens")
                .and_then(Value::as_u64)
                .or_else(|| options.get("maxOutputTokens").and_then(Value::as_u64))
            {
                req.max_tokens = max;
            }
            if let Some(extra) = options.get("options").filter(|value| value.is_object()) {
                req.options = extra.clone();
            }
        }
        complete_request(req, None).await?
    };
    app.plugins.emit(serde_json::json!({
        "id": crate::util::id("evt"),
        "type": "session.usage.recorded",
        "properties": {
            "sessionID": handle.id(),
            "source": "title",
            "cost": cost(&sel.model, &usage),
            "tokens": {
                "input": usage.input,
                "output": usage.output,
                "reasoning": usage.reasoning,
                "cache": { "read": usage.cache_read, "write": usage.cache_write },
            },
        },
    }));
    let title = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches('.')
        .trim_start_matches("Title:")
        .trim()
        .to_string();
    if title.is_empty() {
        return Ok(());
    }
    let title = crate::util::truncate_chars(&title, 100);
    handle.with(|s| s.title = title.clone());
    handle.save();
    Ok(())
}
