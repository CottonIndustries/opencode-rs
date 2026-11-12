//! Built-in local artifact tools.

use super::{req_str, Context, Output, Tool};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

fn meta(result: &Value) -> Value {
    json!({ "artifact": result })
}

pub struct Create;

#[async_trait]
impl Tool for Create {
    fn name(&self) -> &str {
        "artifact_create"
    }
    fn description(&self) -> String {
        "Publish a local artifact from a file map. Use for HTML reports, dashboards, or other static output.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "files": { "type": "object", "additionalProperties": { "type": "string" } },
                "entry": { "type": "string" },
                "provider": { "type": "string" }
            },
            "required": ["title", "files"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let title = req_str(&input, "title")?;
        let mut request = input.clone();
        request["sessionID"] = json!(ctx.session_id);
        let result = crate::artifact::create(&request)?;
        let url = result.get("url").and_then(Value::as_str).unwrap_or_default();
        ctx.app.plugins.emit(json!({
            "id": crate::util::id("evt"),
            "type": "artifact.updated",
            "properties": {}
        }));
        Ok(Output::text(format!("Published {title}: {url}"), title).meta(meta(&result)))
    }
}

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "artifact_read"
    }
    fn description(&self) -> String {
        "Read an artifact's metadata and file contents.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, _ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        let content = crate::artifact::get(id)?;
        let text = serde_json::to_string_pretty(&content)?;
        Ok(Output::text(text, id).meta(meta(&content)))
    }
}

pub struct Update;

#[async_trait]
impl Tool for Update {
    fn name(&self) -> &str {
        "artifact_update"
    }
    fn description(&self) -> String {
        "Replace an artifact's file map and optionally its title or entry file.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" },
                "files": { "type": "object", "additionalProperties": { "type": "string" } },
                "entry": { "type": "string" }
            },
            "required": ["id", "files"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, _ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        let result = crate::artifact::update(id, &input)?;
        let url = result.get("url").and_then(Value::as_str).unwrap_or_default();
        Ok(Output::text(format!("Updated {id}: {url}"), id).meta(meta(&result)))
    }
}

pub struct Delete;

#[async_trait]
impl Tool for Delete {
    fn name(&self) -> &str {
        "artifact_delete"
    }
    fn description(&self) -> String {
        "Delete an artifact.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, _ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id")?;
        let deleted = crate::artifact::delete(id)?;
        Ok(Output::text(format!("Deleted {id}: {deleted}"), id).meta(json!({ "id": id, "deleted": deleted })))
    }
}
