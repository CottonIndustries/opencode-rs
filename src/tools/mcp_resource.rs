//! MCP resource discovery and reads.

use super::{req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ListResources;

#[async_trait]
impl Tool for ListResources {
    fn name(&self) -> &str {
        "list_mcp_resources"
    }

    fn description(&self) -> String {
        "List documents, records, and other data exposed by MCP servers. Use this when the user refers to something that is not a local file, such as a URI with a custom scheme, then load a match with read_mcp_resource. Each entry names the server to read it from. Templates are resources addressed by a parameter such as a record ID; fill in the uriTemplate placeholders before reading.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "MCP server name as configured. Omit to list every server."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let server = input.get("server").and_then(Value::as_str);
        let servers = match server {
            Some(server) => vec![server.to_string()],
            None => ctx.app.mcp.server_names(),
        };
        ctx.ask(
            "opencode_list_mcp_resources",
            &servers,
            &servers,
            "List MCP resources".into(),
            json!({ "server": server }),
        )
        .await?;
        let catalog = ctx.app.mcp.resource_catalog(server).await?;
        let text = serde_json::to_string_pretty(&catalog)?;
        Ok(Output::text(text, "MCP resources").meta(catalog))
    }
}

pub struct ReadResource;

#[async_trait]
impl Tool for ReadResource {
    fn name(&self) -> &str {
        "read_mcp_resource"
    }

    fn description(&self) -> String {
        "Read one MCP resource by server and URI. Not for local files. Always return the full contents rather than slicing or filtering them; oversized output is truncated automatically and the full content is saved to a file you can read. Images and PDFs are shown to you directly.".into()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "The server field of the discovered resource." },
                "uri": { "type": "string", "description": "Exact resource URI from discovery, an expanded uriTemplate, or the user." }
            },
            "required": ["server", "uri"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let server = req_str(&input, "server")?;
        let uri = req_str(&input, "uri")?;
        ctx.ask(
            "opencode_read_mcp_resource",
            &[format!("{server}:{uri}")],
            &[format!("{server}:*")],
            format!("Read MCP resource {server}:{uri}"),
            json!({ "server": server, "uri": uri }),
        )
        .await?;
        let resource = ctx
            .app
            .mcp
            .read_resource(server, uri)
            .await?
            .ok_or_else(|| anyhow!("MCP server \"{server}\" is not connected or does not expose resources"))?;
        let mut out = Output::text("", format!("{server}:{uri}"));
        let mut texts = Vec::new();
        for content in resource.get("contents").and_then(Value::as_array).into_iter().flatten() {
            let mime = content.get("mimeType").and_then(Value::as_str).unwrap_or("");
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                texts.push(text.to_string());
            } else if let Some(blob) = content.get("blob").and_then(Value::as_str) {
                if mime.starts_with("image/") || mime == "application/pdf" {
                    out.attachments.push(crate::session::FilePart {
                        mime: mime.to_string(),
                        data: blob.to_string(),
                        name: uri.to_string(),
                    });
                    texts.push(format!("[{} attachment: {uri}]", mime));
                } else {
                    texts.push(format!("[binary resource {uri}: {mime}, {} bytes]", blob.len()));
                }
            }
        }
        out.output = texts.join("\n");
        out.metadata = resource;
        Ok(out)
    }
}
