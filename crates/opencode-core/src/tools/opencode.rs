//! Tools for managing OpenCode itself: sessions, worktrees and model search.

use crate::tools::{arg_bool, arg_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

pub struct SessionRename;
pub struct SessionMove;
pub struct WorktreeCreate;
pub struct WorktreeList;
pub struct WorktreeRemove;
pub struct Models;

#[async_trait]
impl Tool for SessionRename {
    fn name(&self) -> &str {
        "opencode_session_rename"
    }
    fn description(&self) -> String {
        "Rename a session, or omit sessionID to rename the current session. Use a short, specific title that summarizes the work being done.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionID": { "type": "string", "description": "Omit to rename the current session." },
                "title": { "type": "string", "minLength": 1, "description": "New session title." }
            },
            "required": ["title"]
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = arg_str(&input, "sessionID").unwrap_or(&ctx.session_id).to_string();
        let title = arg_str(&input, "title").ok_or_else(|| anyhow!("title is required"))?.trim().to_string();
        if title.is_empty() {
            return Err(anyhow!("Session title must not be empty"));
        }
        let handle = ctx.app.session(&id)?;
        handle.with(|session| session.title = title.clone());
        handle.save();
        ctx.app.plugins.emit(json!({
            "id": crate::util::id("evt"),
            "type": "session.updated",
            "properties": { "sessionID": id, "info": { "title": title } }
        }));
        Ok(Output::text(format!("Renamed session {id} to {title}."), title.clone())
            .meta(json!({ "sessionID": id, "title": title })))
    }
}

#[async_trait]
impl Tool for SessionMove {
    fn name(&self) -> &str {
        "opencode_session_move"
    }
    fn description(&self) -> String {
        "Move a session to another directory, or omit sessionID to move the current session. The current session moves at the next safe boundary; do not run destination-dependent tools in the same execute call.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionID": { "type": "string", "description": "Omit to move the current session." },
                "directory": { "type": "string", "minLength": 1, "description": "Destination directory, relative to the target session's directory or absolute. Supports ~." }
            },
            "required": ["directory"]
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = arg_str(&input, "sessionID").unwrap_or(&ctx.session_id).to_string();
        let raw = arg_str(&input, "directory").ok_or_else(|| anyhow!("directory is required"))?;
        let handle = ctx.app.session(&id)?;
        let directory = crate::util::resolve(&ctx.directory(), raw);
        if !directory.is_dir() {
            return Err(anyhow!("Unable to move session to {raw}: destination does not exist"));
        }
        let inside = crate::util::contains(&ctx.app.worktree, &directory)
            || crate::util::contains(&ctx.directory(), &directory)
            || crate::util::contains(&crate::paths::get().tmp, &directory);
        if !inside {
            let pattern = format!("{}{}*", directory.display(), std::path::MAIN_SEPARATOR);
            ctx.ask(
                "external_directory",
                &[directory.display().to_string()],
                &[pattern],
                format!("Access path outside the project: {}", directory.display()),
                json!({ "filepath": directory.display().to_string(), "parentDir": directory.display().to_string() }),
            )
            .await?;
        }
        crate::plugin::pty::manager().kill(&id);
        crate::provider::ws_transport::close(&id).await;
        handle.with(|session| {
            let previous = session.directory.clone();
            session.record(json!({
                "type": "location-switched",
                "location": { "directory": directory.to_string_lossy() },
                "projectID": crate::session::project_id(&directory),
                "previous": { "location": { "directory": previous.to_string_lossy() } },
            }));
            session.directory = directory.clone();
            session.project = crate::session::project_id(&directory);
        });
        handle.save();
        ctx.app.plugins.emit(json!({
            "id": crate::util::id("evt"),
            "type": "session.moved",
            "properties": {
                "sessionID": id,
                "projectID": crate::session::project_id(&directory),
                "location": { "directory": directory.to_string_lossy() }
            }
        }));
        Ok(Output::text(
            format!("Moved session {id} to {}.", directory.display()),
            directory.display().to_string(),
        )
        .meta(json!({ "sessionID": id, "directory": directory.display().to_string() })))
    }
}

#[async_trait]
impl Tool for WorktreeCreate {
    fn name(&self) -> &str {
        "opencode_worktree_create"
    }
    fn description(&self) -> String {
        "Create a named worktree. Returns its directory.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Worktree name." } },
            "required": ["name"]
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let name = arg_str(&input, "name").ok_or_else(|| anyhow!("name is required"))?.trim();
        if name.is_empty() {
            return Err(anyhow!("name is required"));
        }
        let value = crate::vcs::dispatch(
            "worktree.create",
            &json!({ "sourceDirectory": ctx.directory().to_string_lossy(), "name": name }),
        )?;
        let directory = value.get("directory").and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Output::text(format!("Created worktree in {directory}."), directory.clone()).meta(value))
    }
}

#[async_trait]
impl Tool for WorktreeList {
    fn name(&self) -> &str {
        "opencode_worktree_list"
    }
    fn description(&self) -> String {
        "List a repository's worktrees.".into()
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _input: Value, ctx: &Context) -> Result<Output> {
        let value = crate::vcs::dispatch(
            "worktree.list",
            &json!({ "sourceDirectory": ctx.directory().to_string_lossy() }),
        )?;
        Ok(Output::text(serde_json::to_string_pretty(&value).unwrap_or_default(), "Worktrees")
            .meta(json!({ "worktrees": value })))
    }
}

#[async_trait]
impl Tool for WorktreeRemove {
    fn name(&self) -> &str {
        "opencode_worktree_remove"
    }
    fn description(&self) -> String {
        "Remove a worktree. The repository location selects configuration; directory identifies the worktree to remove.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "directory": { "type": "string", "minLength": 1, "description": "Absolute path of the worktree to remove." },
                "force": { "type": "boolean", "description": "Discard modified and untracked files. Only set this when the user has confirmed forced removal." }
            },
            "required": ["directory"]
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let raw = arg_str(&input, "directory").ok_or_else(|| anyhow!("directory is required"))?;
        let directory = crate::util::resolve(&ctx.directory(), raw);
        ctx.ask(
            "edit",
            &[directory.display().to_string()],
            &["*".to_string()],
            format!("Remove worktree: {}", directory.display()),
            json!({ "directory": directory.display().to_string() }),
        )
        .await?;
        crate::vcs::dispatch(
            "worktree.remove",
            &json!({
                "sourceDirectory": ctx.directory().to_string_lossy(),
                "directory": directory.to_string_lossy(),
                "force": arg_bool(&input, "force").unwrap_or(false)
            }),
        )?;
        Ok(Output::text(
            format!("Removed worktree {}.", directory.display()),
            directory.display().to_string(),
        )
        .meta(json!({ "directory": directory.display().to_string(), "removed": true })))
    }
}

#[async_trait]
impl Tool for Models {
    fn name(&self) -> &str {
        "opencode_models"
    }
    fn description(&self) -> String {
        "Search the models available to use. Use this to turn a model name the user mentions into an exact reference for a tool that takes a model. Check your own provider first.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Text to search for in model names and IDs." },
                "provider": { "type": "string", "description": "Provider ID or name to filter by. Try your own provider first." },
                "type": { "type": "string", "enum": ["chat", "image"], "description": "Which kind of model to list. Defaults to chat." },
                "all": { "type": "boolean", "description": "Include older versions of each model family." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum number of models to return. Defaults to 20." },
                "offset": { "type": "integer", "minimum": 0, "description": "Number of models to skip." }
            }
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let kind = arg_str(&input, "type").unwrap_or("chat");
        let query = arg_str(&input, "query").map(|q| q.to_lowercase()).unwrap_or_default();
        let terms: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
        let provider_filter = arg_str(&input, "provider").map(|p| p.to_lowercase());
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let own = ctx.model.provider_id.clone();
        let mut matching: Vec<(String, String, crate::models::Model)> = Vec::new();
        for provider in ctx.app.providers.read().unwrap().values() {
            let models = if kind == "image" { &provider.media_models } else { &provider.models };
            for model in models.values() {
                if let Some(filter) = &provider_filter {
                    if provider.id.to_lowercase() != *filter && provider.name.to_lowercase() != *filter {
                        continue;
                    }
                }
                let text = format!("{}/{} {}", provider.id, model.id, model.name).to_lowercase();
                if !terms.iter().all(|term| text.contains(term)) {
                    continue;
                }
                matching.push((provider.id.clone(), provider.name.clone(), model.clone()));
            }
        }
        matching.sort_by(|left, right| {
            (right.0 != own).cmp(&(left.0 != own))
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| right.2.release_date.cmp(&left.2.release_date))
        });
        if !arg_bool(&input, "all").unwrap_or(false) {
            let mut seen = std::collections::BTreeSet::new();
            matching.retain(|(provider_id, _, model)| {
                seen.insert((provider_id.clone(), model.family.clone()))
            });
        }
        let total = matching.len();
        let page: Vec<_> = matching.into_iter().skip(offset).take(limit).collect();
        let mut providers: Vec<Value> = Vec::new();
        for (provider_id, provider_name, _) in &page {
            if providers.iter().any(|entry| entry.get("id").and_then(Value::as_str) == Some(provider_id)) {
                continue;
            }
            let models: Vec<Value> = page
                .iter()
                .filter(|(id, _, _)| id == provider_id)
                .map(|(_, _, model)| {
                    json!({
                        "id": format!("{}/{}", model.provider_id, model.id),
                        "name": model.name,
                        "released": 0,
                        "variants": model.variants(),
                        "cost": {
                            "input": model.cost_input,
                            "output": model.cost_output,
                            "cache": { "read": model.cost_cache_read, "write": model.cost_cache_write }
                        },
                        "status": model.status.clone().unwrap_or_else(|| "active".into())
                    })
                })
                .collect();
            providers.push(json!({ "id": provider_id, "name": provider_name, "models": models }));
        }
        let next = if offset + limit < total { json!(offset + limit) } else { Value::Null };
        let output = json!({ "providers": providers, "total": total, "next": next });
        let mut summary = Map::new();
        summary.insert("providers".into(), output["providers"].clone());
        summary.insert("total".into(), json!(total));
        summary.insert("next".into(), next.clone());
        Ok(Output::text(
            serde_json::to_string_pretty(&output).unwrap_or_default(),
            format!("{total} models"),
        )
        .meta(Value::Object(summary)))
    }
}
