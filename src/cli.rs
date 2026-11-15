//! Auxiliary CLI subcommands: models, auth, session, stats, mcp, debug.

use crate::app::App;
use crate::auth::Credential;
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};

fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| ".".into())
}

pub async fn models(provider: Option<String>, verbose: bool) -> Result<()> {
    let app = App::init(cwd()).await?;
    let all = app.providers();
    let available = crate::models::available(&app.config(), &all);
    let mut ids: Vec<&String> = all.keys().filter(|k| provider.as_ref().map_or(available.contains(k), |p| *k == p)).collect();
    ids.sort();
    if ids.is_empty() {
        if let Some(p) = provider {
            return Err(anyhow!("provider not found: {p}"));
        }
        eprintln!("No providers with credentials found. Set an API key environment variable or run `opencode auth login`.");
    }
    for id in ids {
        let p = &all[id];
        let mut ms: Vec<_> = p.models.values().collect();
        ms.sort_by(|a, b| a.id.cmp(&b.id));
        for m in ms {
            if verbose {
                println!(
                    "{}/{}\t{}\tcontext={} output={} reasoning={} tools={} cost=${}/${}{}",
                    id,
                    m.id,
                    m.name,
                    m.context,
                    m.output,
                    m.reasoning,
                    m.tool_call,
                    m.cost_input,
                    m.cost_output,
                    if m.variants().is_empty() { String::new() } else { format!(" variants={}", m.variants().join(",")) }
                );
            } else {
                println!("{id}/{}", m.id);
            }
        }
    }
    Ok(())
}

pub async fn auth_list() -> Result<()> {
    let creds = crate::auth::all();
    println!("Credentials {}", crate::paths::get().auth_file().display());
    if creds.is_empty() {
        println!("  (none)");
    }
    for (k, c) in &creds {
        println!("  {k}  {}", c.kind());
    }
    let catalog = crate::models::catalog();
    let mut env_found = Vec::new();
    if let Some(obj) = catalog.as_object() {
        for (id, p) in obj {
            for e in p.get("env").and_then(|e| e.as_array()).into_iter().flatten() {
                if let Some(name) = e.as_str() {
                    if std::env::var(name).is_ok_and(|v| !v.is_empty()) {
                        env_found.push(format!("  {id}  {name}"));
                    }
                }
            }
        }
    }
    println!("\nEnvironment");
    if env_found.is_empty() {
        println!("  (none)");
    }
    for l in env_found {
        println!("{l}");
    }
    Ok(())
}

fn prompt_line(label: &str) -> Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

pub async fn auth_login(provider: Option<String>, key: Option<String>, server: Option<String>) -> Result<()> {
    let catalog = crate::models::catalog();
    let provider = match provider {
        Some(p) => p,
        None => {
            let popular = ["anthropic", "openai", "google", "openrouter", "opencode", "groq", "xai", "deepseek", "mistral"];
            eprintln!("Popular providers: {}", popular.join(", "));
            prompt_line("Provider: ")?
        }
    };
    if provider.is_empty() {
        return Err(anyhow!("no provider given"));
    }
    if key.is_none()
        && matches!(
            provider.as_str(),
            "openai" | "chatgpt" | "github-copilot" | "copilot" | "opencode" | "opencode-console" | "console"
        )
    {
        return crate::provider::oauth::login(&provider, server.as_deref()).await;
    }
    if catalog.get(&provider).is_none() {
        eprintln!("Note: {provider} is not in the models.dev catalog; configure it under \"provider\" in opencode.json.");
    }
    if let Some(env) = catalog.get(&provider).and_then(|p| p.get("env")).and_then(|e| e.as_array()) {
        let names: Vec<&str> = env.iter().filter_map(|e| e.as_str()).collect();
        if !names.is_empty() {
            eprintln!("Tip: you can also set {} instead.", names.join(" or "));
        }
    }
    let key = match key {
        Some(k) => k,
        None => {
            if !std::io::stdin().is_terminal() {
                let mut s = String::new();
                std::io::stdin().lock().read_line(&mut s)?;
                s.trim().to_string()
            } else {
                prompt_line("API key: ")?
            }
        }
    };
    if key.is_empty() {
        return Err(anyhow!("no API key given"));
    }
    crate::auth::set(&provider, Credential::Api { key, metadata: None })?;
    println!("Saved credentials for {provider}");
    Ok(())
}

pub async fn auth_logout(provider: Option<String>) -> Result<()> {
    let provider = match provider {
        Some(p) => p,
        None => {
            let creds = crate::auth::all();
            if creds.is_empty() {
                println!("No stored credentials.");
                return Ok(());
            }
            eprintln!("Stored: {}", creds.keys().cloned().collect::<Vec<_>>().join(", "));
            prompt_line("Provider to log out: ")?
        }
    };
    if crate::auth::remove(&provider)? {
        if provider == "opencode" || provider == "console" {
            crate::console::clear();
        }
        println!("Removed credentials for {provider}");
    } else {
        println!("No credentials stored for {provider}");
    }
    Ok(())
}

pub fn auth_export(target: Option<&str>) -> Result<()> {
    let entries: Vec<_> = crate::auth::entries()
        .into_iter()
        .filter(|entry| target.is_none_or(|target| entry.provider == target || entry.id == target || entry.label == target))
        .collect();
    if entries.is_empty() {
        return Err(anyhow!("no credentials found"));
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("Warning: the output contains secrets; redirect it to a file or pipe it to auth import");
    }
    let values: Vec<_> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "provider": entry.provider,
                "label": entry.label,
                "value": entry.credential,
                "active": entry.active,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&values)?);
    Ok(())
}

pub fn auth_import(file: Option<&str>) -> Result<()> {
    let text = match file {
        Some(file) => std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?,
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                return Err(anyhow!("pass a file or pipe auth export output into stdin"));
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            text
        }
    };
    let values: Vec<serde_json::Value> = serde_json::from_str(&text).context("parsing credentials")?;
    let (imported, skipped) = crate::auth::import(&values)?;
    println!(
        "Imported {imported} credential{}{}",
        if imported == 1 { "" } else { "s" },
        if skipped == 0 { String::new() } else { format!(", skipped {skipped}") }
    );
    Ok(())
}

pub fn auth_switch(target: &str, credential: &str) -> Result<()> {
    let entry = crate::auth::switch(target, credential)?;
    println!("Switched {} to {} ({})", entry.provider, entry.label, entry.id);
    Ok(())
}

pub async fn auth_sync() -> Result<()> {
    let summary = crate::console::sync().await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn ago(ms: i64) -> String {
    let d = (crate::util::now_ms() - ms) / 1000;
    match d {
        d if d < 60 => format!("{d}s ago"),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86400 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86400),
    }
}

pub async fn session_list(max: Option<usize>, format: &str) -> Result<()> {
    let app = App::init(cwd()).await?;
    let list: Vec<_> = crate::session::list(&app.project).into_iter().filter(|s| s.parent_id.is_none()).take(max.unwrap_or(100)).collect();
    if format == "json" {
        let v: Vec<_> = list
            .iter()
            .map(|s| json!({ "id": s.id, "title": s.title, "created": s.time_created, "updated": s.time_updated, "directory": s.directory }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("No sessions found.");
    }
    for s in list {
        println!("{}  {:<10}  {}", s.id, ago(s.time_updated), s.title);
    }
    Ok(())
}

pub async fn session_delete(id: &str) -> Result<()> {
    let app = App::init(cwd()).await?;
    let n = crate::session::delete(&app.project, id)?;
    println!("Deleted {n} session(s)");
    Ok(())
}

pub async fn session_export(id: Option<String>, sanitize: bool) -> Result<()> {
    let app = App::init(cwd()).await?;
    let id = match id {
        Some(i) => i,
        None => crate::session::list(&app.project).into_iter().find(|s| s.parent_id.is_none()).map(|s| s.id).ok_or_else(|| anyhow!("no sessions"))?,
    };
    let mut s = crate::session::load(&app.project, &id)?;
    if sanitize {
        s.sanitize();
    }
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(())
}

pub async fn session_import(file: &str, replace: bool) -> Result<()> {
    let app = App::init(cwd()).await?;
    let text = if file.starts_with("http://") || file.starts_with("https://") {
        crate::http::send(crate::http::client().get(file)).await?.error_for_status()?.text().await?
    } else {
        std::fs::read_to_string(file)?
    };
    let mut s: crate::session::Session = serde_json::from_str(&text)?;
    if !replace && crate::session::load(&app.project, &s.id).is_ok() {
        return Err(anyhow!("session already exists: {} (use --replace)", s.id));
    }
    s.project = app.project.clone();
    s.directory = app.directory.clone();
    crate::session::save(&s)?;
    println!("Imported session {} ({})", s.id, s.title);
    Ok(())
}

pub async fn stats(days: Option<i64>, as_json: bool) -> Result<()> {
    let data = crate::stats::data(days)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else {
        print!("{}", crate::stats::format(&data));
    }
    Ok(())
}

pub async fn pair(url: &str, password: Option<String>) -> Result<()> {
    use base64::Engine;
    let mut request = crate::http::client()
        .post(format!("{}/api/pair", url.trim_end_matches('/')))
        .header("accept", "application/json");
    if let Some(password) = password.filter(|password| !password.is_empty()) {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        request = request.header("authorization", format!("Basic {token}"));
    }
    let response = crate::http::send(request).await?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!("pairing failed: HTTP {}", response.status().as_u16()));
    }
    let value = response.json::<serde_json::Value>().await?;
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("pairing response had no code"))?;
    println!("{}/auth/connect/{code}", url.trim_end_matches('/'));
    Ok(())
}

pub async fn mcp_list() -> Result<()> {
    let app = App::init(cwd()).await?;
    if app.mcp.configured() == 0 {
        println!("No MCP servers configured. Add them under \"mcp\" in opencode.json.");
        return Ok(());
    }
    for (name, st) in app.mcp.status().await {
        let auth = if crate::mcp_oauth::logged_in(&name) { "  oauth" } else { "" };
        match st {
            crate::mcp::Status::Connected { tools } => println!("✓ {name}  connected ({tools} tools){auth}"),
            crate::mcp::Status::Disabled => println!("○ {name}  disabled{auth}"),
            crate::mcp::Status::Failed(e) => println!("✗ {name}  failed: {e}{auth}"),
            crate::mcp::Status::NeedsAuth(_) => println!("! {name}  sign in required (opencode mcp auth {name}){auth}"),
        }
    }
    Ok(())
}

pub async fn mcp_auth(name: &str) -> Result<()> {
    let directory = cwd();
    let loaded = crate::config::load(&directory, &directory)?;
    let config = loaded
        .config
        .mcp
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server not configured: {name}"))?;
    crate::mcp_oauth::authorize(name, config).await
}

pub fn mcp_logout(name: &str) -> Result<()> {
    if crate::mcp_oauth::logout(name)? {
        println!("Removed OAuth credentials for {name}");
    } else {
        println!("No OAuth credentials stored for {name}");
    }
    Ok(())
}

pub fn debug_paths(name: Option<&str>) -> Result<()> {
    let p = crate::paths::get();
    let entries = [("home", &p.home), ("data", &p.data), ("cache", &p.cache), ("config", &p.config), ("state", &p.state), ("tmp", &p.tmp), ("log", &p.log)];
    if let Some(name) = name {
        let value = entries.iter().find(|(key, _)| *key == name).map(|(_, value)| value);
        let Some(value) = value else {
            return Err(anyhow!("unknown path {name}; expected one of home, data, cache, config, state, tmp, log"));
        };
        println!("{}", value.display());
        return Ok(());
    }
    for (k, v) in entries {
        println!("{k:<8} {}", v.display());
    }
    Ok(())
}

pub async fn debug_heap_dump() -> Result<()> {
    let app = crate::app::App::init(std::env::current_dir()?).await?;
    let dump = crate::debug_dump::dump(&app)?;
    println!("{}", serde_json::to_string_pretty(&dump)?);
    Ok(())
}

pub fn mcp_add(
    name: &str,
    url: Option<String>,
    headers: &[String],
    environment: &[String],
    global: bool,
    command: &[String],
) -> Result<()> {
    if url.is_some() && !command.is_empty() {
        return Err(anyhow!("provide either --url or a command after --, not both"));
    }
    if url.is_none() && command.is_empty() {
        return Err(anyhow!("provide either --url or a command after --"));
    }
    if url.is_some() && !environment.is_empty() {
        return Err(anyhow!("--env is only valid for local MCP servers"));
    }
    if !command.is_empty() && !headers.is_empty() {
        return Err(anyhow!("--header is only valid for remote MCP servers"));
    }
    let values = |items: &[String]| -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut map = serde_json::Map::new();
        for item in items {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| anyhow!("expected name=value, got {item}"))?;
            map.insert(key.to_string(), serde_json::json!(value));
        }
        Ok(map)
    };
    let mut server = match url {
        Some(url) => {
            if url::Url::parse(&url).is_err() {
                return Err(anyhow!("invalid URL: {url}"));
            }
            let mut server = serde_json::json!({ "type": "remote", "url": url });
            let headers = values(headers)?;
            if !headers.is_empty() {
                server["headers"] = serde_json::Value::Object(headers);
            }
            server
        }
        None => {
            let mut server = serde_json::json!({ "type": "local", "command": command });
            let environment = values(environment)?;
            if !environment.is_empty() {
                server["environment"] = serde_json::Value::Object(environment);
            }
            server
        }
    };
    if !name.trim().is_empty() {
        server["enabled"] = serde_json::json!(true);
    }
    let path = if global {
        crate::paths::get().config.join("opencode.json")
    } else {
        cwd().join("opencode.json")
    };
    let mut config = read_plugin_config(&path)?;
    let object = config.as_object_mut().unwrap();
    let mcp = object.entry("mcp").or_insert_with(|| serde_json::json!({}));
    let mcp = mcp.as_object_mut().ok_or_else(|| anyhow!("mcp configuration must be an object"))?;
    let servers = mcp.entry("servers").or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcp.servers configuration must be an object"))?;
    servers.insert(name.to_string(), server);
    write_plugin_config(&path, &config)?;
    println!("MCP server \"{name}\" added to {}", path.display());
    Ok(())
}

fn plugin_config_path() -> std::path::PathBuf {
    crate::paths::get().config.join("opencode.json")
}

fn read_plugin_config(path: &std::path::Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: serde_json::Value = serde_json::from_str(&text).context("parsing plugin configuration")?;
            if !value.is_object() {
                return Err(anyhow!("plugin configuration must be an object"));
            }
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(error) => Err(error.into()),
    }
}

fn write_plugin_config(path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn plugin_entry_matches(entry: &serde_json::Value, package: &str) -> bool {
    entry.as_str() == Some(package) || entry.get("package").and_then(|value| value.as_str()) == Some(package)
}

pub async fn plugin_list() -> Result<()> {
    let app = App::init(cwd()).await?;
    for error in app.plugins.errors() {
        eprintln!("warning: {error}");
    }
    let plugins = app.plugins.list();
    if plugins.is_empty() {
        println!("No plugins configured.");
    } else {
        println!("{}", serde_json::to_string_pretty(&plugins)?);
    }
    Ok(())
}

pub fn plugin_add(package: &str) -> Result<()> {
    if package.trim().is_empty() {
        return Err(anyhow!("plugin package is required"));
    }
    let path = plugin_config_path();
    let mut config = read_plugin_config(&path)?;
    let plugins = config
        .as_object_mut()
        .unwrap()
        .entry("plugins")
        .or_insert_with(|| serde_json::json!([]));
    let array = plugins.as_array_mut().ok_or_else(|| anyhow!("plugins configuration must be an array"))?;
    if array.iter().any(|entry| plugin_entry_matches(entry, package)) {
        println!("Plugin \"{package}\" is already configured in {}", path.display());
        return Ok(());
    }
    array.push(serde_json::json!(package));
    write_plugin_config(&path, &config)?;
    println!("Plugin \"{package}\" added to {}", path.display());
    Ok(())
}

pub fn plugin_remove(package: &str) -> Result<()> {
    let path = plugin_config_path();
    let mut config = read_plugin_config(&path)?;
    let Some(plugins) = config.get_mut("plugins").and_then(|value| value.as_array_mut()) else {
        println!("Plugin \"{package}\" is not configured");
        return Ok(());
    };
    let before = plugins.len();
    plugins.retain(|entry| !plugin_entry_matches(entry, package));
    if plugins.len() == before {
        println!("Plugin \"{package}\" is not configured");
        return Ok(());
    }
    write_plugin_config(&path, &config)?;
    println!("Plugin \"{package}\" removed from {}", path.display());
    Ok(())
}

pub async fn plugin_check(target: Option<&str>) -> Result<()> {
    let app = App::init(cwd()).await?;
    let items = crate::plugin_package::inspect(&app, target).await?;
    if items.is_empty() {
        println!("No package plugins found");
        return Ok(());
    }
    for runtime in ["Server", "TUI"] {
        let rows: Vec<_> = items.iter().filter(|item| item.runtime == runtime).collect();
        if rows.is_empty() {
            continue;
        }
        println!("{runtime}");
        for item in rows {
            let version = item.version.as_deref().map(|version| format!(" {version}")).unwrap_or_default();
            let status = if item.error.is_some() {
                "check failed"
            } else if item.outdated {
                "update available"
            } else {
                "current"
            };
            println!("  {}{} ({status})", item.name, version);
        }
    }
    if items.iter().any(|item| item.error.is_some()) {
        return Err(anyhow!("one or more plugins could not be checked"));
    }
    Ok(())
}

pub async fn plugin_update(target: Option<&str>) -> Result<()> {
    let app = App::init(cwd()).await?;
    let items = crate::plugin_package::inspect(&app, target).await?;
    for item in items.iter().filter(|item| item.error.is_some()) {
        eprintln!(
            "Failed to check {} plugin {:?}: {}",
            item.runtime,
            item.name,
            item.error.as_deref().unwrap_or_default()
        );
    }
    let targets: Vec<String> = target.map(|target| vec![target.to_string()]).unwrap_or_default();
    let updated = crate::plugin_package::update(&app, &targets).await?;
    if updated.is_empty() {
        println!("No plugin updates available");
    } else {
        for target in updated {
            println!("Updated plugin {:?}", target);
        }
    }
    Ok(())
}

pub async fn debug_plugins() -> Result<()> {
    std::env::set_var("OPENCODE_TUI", "1");
    let app = App::init(cwd()).await?;
    for error in app.plugins.errors() {
        eprintln!("warning: {error}");
    }
    let plugins = app.plugins.list();
    if plugins.is_empty() {
        println!("No plugins loaded.");
    } else {
        println!("{}", serde_json::to_string_pretty(&plugins)?);
    }
    let tools = app.plugins.tool_descriptors();
    println!(
        "\nTools: {}",
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    let websearch = app.plugins.websearch_providers();
    if !websearch.is_empty() {
        println!(
            "Web search: {}",
            websearch.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    let tui = app.plugins.tui_commands();
    if !tui.is_empty() {
        println!(
            "TUI commands: {}",
            tui.iter()
                .filter_map(|command| command.get("id").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let pages = app.plugins.tui_pages();
    if !pages.is_empty() {
        println!(
            "TUI pages: {}",
            pages
                .iter()
                .filter_map(|page| page.get("name").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let slots = app
        .plugins
        .tui_slots(serde_json::json!({ "sessionID": "debug", "mode": "normal", "showDetails": true }))
        .await;
    if !slots.is_empty() {
        println!("{}", serde_json::to_string_pretty(&slots)?);
    }
    Ok(())
}

pub async fn debug_plugin_tool(name: &str, input: &str) -> Result<()> {
    let app = App::init(cwd()).await?;
    let input: serde_json::Value = serde_json::from_str(input).context("parsing tool input")?;
    let result = app
        .plugins
        .execute_tool(
            name,
            "call_debug",
            input,
            serde_json::json!({
                "sessionID": "debug",
                "agent": app.default_agent(),
                "messageID": "msg_debug",
                "id": "call_debug",
                "directory": app.directory.to_string_lossy(),
                "worktree": app.worktree.to_string_lossy(),
            }),
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub async fn debug_plugin_tui(name: &str, input: &str) -> Result<()> {
    std::env::set_var("OPENCODE_TUI", "1");
    let app = App::init(cwd()).await?;
    let result = app.plugins.execute_tui_command(name, serde_json::json!(input)).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub fn debug_events(session: Option<&str>) -> Result<()> {
    let events = crate::event_store::read(session, None, Some(100));
    println!("{}", serde_json::to_string_pretty(&events)?);
    Ok(())
}

pub async fn debug_config() -> Result<()> {
    let app = App::init(cwd()).await?;
    eprintln!("Files: {}", app.config_files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", "));
    println!("{}", serde_json::to_string_pretty(&app.config())?);
    Ok(())
}

pub async fn debug_agents() -> Result<()> {
    let app = App::init(cwd()).await?;
    let agents = app.agents();
    for a in agents.values() {
        println!(
            "{:<12} {:<9} {}{}",
            a.id,
            match a.mode {
                crate::agent::Mode::Primary => "primary",
                crate::agent::Mode::Subagent => "subagent",
                crate::agent::Mode::All => "all",
            },
            a.description.as_deref().unwrap_or(""),
            if a.hidden { " (hidden)" } else { "" }
        );
    }
    Ok(())
}

pub async fn debug_skills() -> Result<()> {
    let app = App::init(cwd()).await?;
    for s in app.skills() {
        println!("{:<16} {}  [{}]", s.id, crate::util::truncate_chars(&s.description, 80), s.path.display());
    }
    Ok(())
}

pub async fn debug_prompt() -> Result<()> {
    let app = App::init(cwd()).await?;
    let agent = app.agent(&app.default_agent())?.clone();
    let sel = crate::engine::select_model(&app, &agent, &Default::default(), None)?;
    let tools = crate::tools::registry(&app, &agent, &sel.model).await;
    let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    println!("# model: {}  tools: {}\n", sel.model.full_id(), names.join(", "));
    for s in crate::prompt::system(&app, &agent, &sel.model, &names).await {
        println!("{s}\n---");
    }
    Ok(())
}

pub fn service_start(port: u16, directory: Option<std::path::PathBuf>) -> Result<()> {
    let info = crate::service::start(port, directory.unwrap_or_else(cwd))?;
    println!("Started opencode at {}", info["url"].as_str().unwrap_or_default());
    println!("PID {}  log {}", info["pid"], info["log"].as_str().unwrap_or_default());
    Ok(())
}

pub fn service_restart(port: u16, directory: Option<std::path::PathBuf>) -> Result<()> {
    let _ = crate::service::stop();
    service_start(port, directory)
}

pub fn service_stop() -> Result<()> {
    let info = crate::service::stop()?;
    if info["stopped"].as_bool().unwrap_or(false) {
        println!("Stopped server PID {}", info["pid"]);
    } else {
        println!("No running server");
    }
    Ok(())
}

pub fn service_status() -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&crate::service::status())?);
    Ok(())
}

pub fn workspace_list() -> Result<()> {
    let workspaces = crate::workspace::list();
    if workspaces.is_empty() {
        println!("(none)");
    }
    for workspace in workspaces {
        println!("{:<24} {:<10} {}", workspace.id, workspace.provider, workspace.directory);
    }
    Ok(())
}

pub fn workspace_create(
    directory: Option<std::path::PathBuf>,
    provider: &str,
    id: Option<&str>,
) -> Result<()> {
    let info = crate::workspace::create(id, provider, &directory.unwrap_or_else(cwd))?;
    println!("Created workspace {} ({}) at {}", info.id, info.provider, info.directory);
    Ok(())
}

pub fn workspace_remove(id: &str) -> Result<()> {
    if crate::workspace::destroy(id)? {
        println!("Removed workspace {id}");
    } else {
        println!("Workspace not found: {id}");
    }
    Ok(())
}

pub fn repository_cache(repository: &str, branch: Option<&str>, refresh: bool) -> Result<()> {
    let value = crate::repository::ensure(repository, branch, refresh)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn migrate() -> Result<()> {
    let result = crate::migration::run();
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub async fn api_call(
    server: &str,
    method: &str,
    path: &str,
    data: Option<&str>,
    password: Option<&str>,
    header: &[String],
    param: &[String],
) -> Result<()> {
    let mut url = format!(
        "{}/{}",
        server.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if !param.is_empty() {
        let query = param
            .iter()
            .filter_map(|item| item.split_once('='))
            .map(|(key, value)| format!("{}={}", encode_query(key), encode_query(value)))
            .collect::<Vec<_>>()
            .join("&");
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&query);
    }
    let client = crate::http::client();
    let mut request = match method.to_ascii_uppercase().as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "PATCH" => client.patch(&url),
        "DELETE" => client.delete(&url),
        other => return Err(anyhow!("unsupported HTTP method: {other}")),
    };
    request = request.header("accept", "application/json");
    for item in header {
        if let Some((name, value)) = item.split_once(':') {
            request = request.header(name.trim(), value.trim());
        }
    }
    if let Some(password) = password {
        use base64::Engine;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        request = request.header("authorization", format!("Basic {token}"));
    }
    if let Some(data) = data {
        let body: serde_json::Value = serde_json::from_str(data)?;
        request = request.header("content-type", "application/json").json(&body);
    }
    let response = crate::http::send(request).await?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(anyhow!("HTTP {status}: {text}"));
    }
    println!("{text}");
    Ok(())
}

fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

