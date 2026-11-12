//! `websearch`: search the web through a configured integration (Exa, Tavily, Parallel, Firecrawl).

use super::{req_str, Context, Output, Tool};
use crate::config::Config;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WebSearch;

const PROVIDERS: &[(&str, &str)] = &[("exa", "EXA_API_KEY"), ("tavily", "TAVILY_API_KEY"), ("parallel", "PARALLEL_API_KEY"), ("firecrawl", "FIRECRAWL_API_KEY")];

pub fn providers() -> Vec<(String, String)> {
    PROVIDERS
        .iter()
        .map(|(id, _)| {
            let name = match *id {
                "exa" => "Exa",
                "tavily" => "Tavily",
                "parallel" => "Parallel",
                "firecrawl" => "Firecrawl",
                other => other,
            };
            ((*id).to_string(), name.to_string())
        })
        .collect()
}

/// Run a built-in provider search and return the provider id plus result rows.
pub async fn execute_builtin(provider: Option<&str>, query: &str) -> Result<(String, Vec<Value>)> {
    let creds = crate::auth::all();
    let key_for = |id: &str, env: &str| {
        std::env::var(env)
            .ok()
            .filter(|key| !key.is_empty())
            .or_else(|| creds.get(id).map(|credential| credential.secret().to_string()))
    };
    let (id, key) = if let Some(id) = provider {
        let env = PROVIDERS
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, env)| *env)
            .ok_or_else(|| anyhow!("unknown web search provider: {id}"))?;
        (id.to_string(), key_for(id, env).ok_or_else(|| anyhow!("Web search provider is not configured: {id}"))?)
    } else {
        PROVIDERS
            .iter()
            .find_map(|(id, env)| key_for(id, env).map(|key| ((*id).to_string(), key)))
            .ok_or_else(|| anyhow!("No web search integration is configured"))?
    };
    let results = search(&id, &key, query).await?;
    let values = results
        .iter()
        .map(|result| json!({
            "url": result.url,
            "title": result.title,
            "content": result.content,
            "time": {},
        }))
        .collect();
    Ok((id, values))
}

/// The selected provider and its key, if any is configured.
pub fn configured(config: &Config) -> Option<(String, String)> {
    let creds = crate::auth::all();
    let key_for = |id: &str, env: &str| std::env::var(env).ok().filter(|k| !k.is_empty()).or_else(|| creds.get(id).map(|c| c.secret().to_string()));
    if let Some(p) = config.websearch.as_ref().and_then(|w| w.provider.clone()) {
        let env = PROVIDERS.iter().find(|(id, _)| *id == p).map(|(_, e)| *e)?;
        return key_for(&p, env).map(|k| (p, k));
    }
    PROVIDERS.iter().find_map(|(id, env)| key_for(id, env).map(|k| (id.to_string(), k)))
}

struct Result_ {
    title: String,
    url: String,
    content: String,
}

async fn search(provider: &str, key: &str, query: &str) -> Result<Vec<Result_>> {
    let http = crate::http::client();
    let (req, path_results, title_k, url_k, content_k) = match provider {
        "exa" => (
            http.post("https://api.exa.ai/search").header("x-api-key", key).json(&json!({ "query": query, "numResults": 8, "type": "auto", "contents": { "text": { "maxCharacters": 3000 } } })),
            "/results", "title", "url", "text",
        ),
        "tavily" => (
            http.post("https://api.tavily.com/search").bearer_auth(key).json(&json!({ "query": query, "max_results": 8, "search_depth": "basic" })),
            "/results", "title", "url", "content",
        ),
        "parallel" => (
            http.post("https://api.parallel.ai/v1beta/search").header("x-api-key", key).json(&json!({ "objective": query, "search_queries": [query], "max_results": 8 })),
            "/results", "title", "url", "excerpts",
        ),
        "firecrawl" => (
            http.post("https://api.firecrawl.dev/v1/search").bearer_auth(key).json(&json!({ "query": query, "limit": 8 })),
            "/data", "title", "url", "description",
        ),
        other => return Err(anyhow!("unknown web search provider: {other}")),
    };
    let resp = crate::http::send(req.timeout(std::time::Duration::from_secs(30))).await?;
    match resp.status().as_u16() {
        429 => return Err(anyhow!("Web search rate limited (HTTP 429)")),
        401 => return Err(anyhow!("Web search authentication failed (HTTP 401)")),
        s if !(200..300).contains(&s) => return Err(anyhow!("Web search failed (HTTP {s}): {}", crate::provider::error_message(&resp.text().await.unwrap_or_default()))),
        _ => {}
    }
    let v: Value = resp.json().await?;
    let items = v.pointer(path_results).and_then(|r| r.as_array()).cloned().unwrap_or_default();
    Ok(items
        .iter()
        .map(|it| {
            let content = match it.get(content_k) {
                Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n"),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            Result_ {
                title: it.get(title_k).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                url: it.get(url_k).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                content,
            }
        })
        .collect())
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> String {
        format!(
            "Search the web using the user's selected search integration. Use this for current information beyond knowledge cutoff.\nThe current year is {}. Use this year when searching for recent information or current events.",
            chrono::Local::now().format("%Y")
        )
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "Websearch query" } },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let query = req_str(&input, "query")?.to_string();
        let requested = ctx.app.config.websearch.as_ref().and_then(|w| w.provider.as_deref());
        let plugin_provider = ctx.app.plugins.websearch_provider(requested);
        let builtin = configured(&ctx.app.config);
        // A provider named in config or selected by a plugin's default wins
        // over an unrelated builtin API key.
        let plugin_selected = plugin_provider.is_some()
            && (requested.is_some() || ctx.app.config.websearch.as_ref().and_then(|w| w.provider.as_ref()).is_none());
        if !plugin_selected {
            if let Some((provider, key)) = builtin {
                return builtin_search(ctx, &provider, &key, &query).await;
            }
        }
        if let Some(provider) = plugin_provider {
            return plugin_search(ctx, &provider, &query).await;
        }
        if let Some((provider, key)) = configured(&ctx.app.config) {
            return builtin_search(ctx, &provider, &key, &query).await;
        }
        Err(anyhow!("No web search integration is configured"))
    }
}

async fn builtin_search(ctx: &Context, provider: &str, key: &str, query: &str) -> Result<Output> {
    ctx.ask("websearch", &[query.to_string()], &["*".into()], format!("Search the web: {query}"), json!({ "query": query, "provider": provider })).await?;
    let results = search(provider, key, query).await?;
    if results.is_empty() {
        return Ok(Output::text("No search results found. Please try a different query.", query));
    }
    let text = results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n{}\n{}", i + 1, r.title, r.url, crate::util::truncate_chars(r.content.trim(), 3000)))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Output::text(text, query).meta(json!({ "provider": provider, "results": results.len() })))
}

async fn plugin_search(ctx: &Context, provider: &str, query: &str) -> Result<Output> {
    ctx.ask("websearch", &[query.to_string()], &["*".into()], format!("Search the web: {query}"), json!({ "query": query, "provider": provider })).await?;
    let response = ctx.app.plugins.execute_websearch(provider, query).await?;
    let results = response.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    if results.is_empty() {
        return Ok(Output::text("No search results found. Please try a different query.", query));
    }
    let text = results
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("");
            let url = item.get("url").and_then(Value::as_str).unwrap_or("");
            let content = item.get("content").and_then(Value::as_str).unwrap_or("");
            format!("{}. {}\n{}\n{}", i + 1, title, url, crate::util::truncate_chars(content.trim(), 3000))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Output::text(text, query).meta(json!({ "provider": provider, "results": results.len() })))
}
