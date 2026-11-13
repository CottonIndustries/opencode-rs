//! Data and plain-text report builders for the TUI's slash-command dialogs.
//!
//! Every function returns plain text suitable for a scrollable Text dialog;
//! none of them emit terminal escape sequences.

use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

fn field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn number(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn stamp(ms: i64) -> String {
    if ms <= 0 {
        return "unknown".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|date| date.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn list<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    value.get(key).and_then(Value::as_array).map(|items| items.iter().collect()).unwrap_or_default()
}

/// Token usage of the last assistant turn plus session totals.
pub fn usage(app: &crate::app::App, handle: &crate::app::Handle) -> String {
    use crate::session::Role;
    let mut out = String::new();
    handle.with(|session| {
        let last = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant && (message.usage.input + message.usage.output + message.usage.cache_read) > 0);
        match last {
            Some(message) => {
                let u = &message.usage;
                let tokens = u.input + u.output + u.reasoning + u.cache_read + u.cache_write;
                let _ = writeln!(out, "Last assistant turn");
                let _ = writeln!(out, "  Model         {}", message.model.clone().unwrap_or_else(|| "unknown".to_string()));
                let _ = writeln!(out, "  Input         {}", u.input);
                let _ = writeln!(out, "  Output        {}", u.output);
                let _ = writeln!(out, "  Reasoning     {}", u.reasoning);
                let _ = writeln!(out, "  Cache read    {}", u.cache_read);
                let _ = writeln!(out, "  Cache write   {}", u.cache_write);
                let _ = writeln!(out, "  Total         {tokens}");
                let context = message
                    .model
                    .as_deref()
                    .and_then(|full| crate::models::resolve(&app.providers.read().unwrap(), full).ok())
                    .map(|selection| selection.model.context)
                    .unwrap_or(0);
                if context > 0 {
                    let percent = (tokens as f64 / context as f64 * 100.0).round() as u64;
                    let _ = writeln!(out, "  Context       {tokens} / {context} ({percent}%)");
                } else {
                    let _ = writeln!(out, "  Context       limit unknown");
                }
                let _ = writeln!(out, "  Reported cost ${:.4}", u.cost.unwrap_or(0.0));
            }
            None => {
                let _ = writeln!(out, "No assistant turn with token usage yet.");
            }
        }
        let (total, cost) = session.usage_total();
        let _ = writeln!(out);
        let _ = writeln!(out, "Session total");
        let _ = writeln!(out, "  Cost          ${cost:.4}");
        let _ = writeln!(out, "  Input         {}", total.input);
        let _ = writeln!(out, "  Output        {}", total.output);
        let _ = writeln!(out, "  Reasoning     {}", total.reasoning);
        let _ = writeln!(out, "  Cache read    {}", total.cache_read);
        let _ = writeln!(out, "  Cache write   {}", total.cache_write);
        let _ = writeln!(out, "  Tokens        {}", total.input + total.output + total.reasoning + total.cache_read + total.cache_write);
    });
    out
}

/// OpenCode Console workspace budgets and Go usage meters.
pub async fn usage_limits() -> String {
    match crate::account::usage_limits().await {
        Ok(None) => "No OpenCode Console account is configured.\nRun `opencode auth login` and choose OpenCode to see usage limits.\n".to_string(),
        Err(error) => format!("Usage limits are unavailable: {error}\n"),
        Ok(Some(info)) => {
            let mut out = String::new();
            let workspace = info.get("workspace").cloned().unwrap_or(Value::Null);
            let _ = writeln!(out, "OpenCode Console");
            let _ = writeln!(out, "  Workspace     {}", field(&workspace, "id"));
            let url = field(&workspace, "usageURL");
            if !url.is_empty() {
                let _ = writeln!(out, "  Usage         {url}");
            }
            let budgets = list(&info, "budgets");
            let _ = writeln!(out);
            if budgets.is_empty() {
                let _ = writeln!(out, "Budgets       none reported");
            } else {
                let _ = writeln!(out, "Budgets");
                for budget in budgets {
                    let limit = number(budget, "limit");
                    let spent = number(budget, "spent");
                    let _ = writeln!(out, "  {}  ${spent:.2} / ${limit:.2}", field(budget, "id"));
                    let scope = [field(budget, "scope"), field(budget, "per"), field(budget, "coverage")]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>();
                    if !scope.is_empty() {
                        let _ = writeln!(out, "    scope       {}", scope.join(" / "));
                    }
                    let vendors = budget
                        .get("vendors")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    if !vendors.is_empty() {
                        let _ = writeln!(out, "    vendors     {}", vendors.join(", "));
                    }
                    let models = budget
                        .get("models")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|model| {
                                    let name = field(model, "name");
                                    let name = if name.is_empty() { field(model, "modelID") } else { name };
                                    match model.get("spent").and_then(Value::as_f64) {
                                        Some(spent) => format!("{name} ${spent:.2}"),
                                        None => name,
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if !models.is_empty() {
                        let _ = writeln!(out, "    models      {}", models.join(", "));
                    }
                    let _ = writeln!(out, "    resets      {}", stamp(budget.get("resetsAt").and_then(Value::as_i64).unwrap_or(0)));
                }
            }
            if let Some(go) = info.get("go") {
                let _ = writeln!(out);
                let _ = writeln!(out, "Go usage limits");
                for key in ["rolling", "weekly", "monthly"] {
                    if let Some(meter) = go.get(key) {
                        let percent = number(meter, "percent");
                        let limited = meter.get("limited").and_then(Value::as_bool).unwrap_or(false);
                        let mut line = format!("  {key:<8} {percent:.0}%");
                        if limited {
                            line.push_str("  (rate limited)");
                        }
                        let _ = writeln!(out, "{line}");
                        let resets = meter.get("resetsAt").and_then(Value::as_i64).unwrap_or(0);
                        if resets > 0 {
                            let _ = writeln!(out, "           resets {}", stamp(resets));
                        }
                    }
                }
            }
            out
        }
    }
}

/// Paths, context, configuration files and registry counts.
pub fn debug(app: &crate::app::App) -> String {
    let paths = crate::paths::get();
    let mut out = String::new();
    let _ = writeln!(out, "Version       {}", crate::VERSION);
    let _ = writeln!(out);
    let _ = writeln!(out, "Paths");
    let _ = writeln!(out, "  Home        {}", paths.home.display());
    let _ = writeln!(out, "  Data        {}", paths.data.display());
    let _ = writeln!(out, "  Cache       {}", paths.cache.display());
    let _ = writeln!(out, "  Config      {}", paths.config.display());
    let _ = writeln!(out, "  State       {}", paths.state.display());
    let _ = writeln!(out, "  Temp        {}", paths.tmp.display());
    let _ = writeln!(out, "  Log         {}", paths.log.display());
    let _ = writeln!(out);
    let _ = writeln!(out, "Context");
    let _ = writeln!(out, "  Project     {}", app.project);
    let _ = writeln!(out, "  Directory   {}", app.directory.display());
    let _ = writeln!(out, "  Worktree    {}", app.worktree.display());
    let _ = writeln!(out, "  Git         {}", if app.is_git { "yes" } else { "no" });
    let _ = writeln!(out, "  Agent       {}", app.default_agent());
    let _ = writeln!(out);
    let _ = writeln!(out, "Configuration files");
    if app.config_files.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for file in &app.config_files {
            let _ = writeln!(out, "  {}", file.display());
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Registries");
    let _ = writeln!(out, "  Providers   {}", app.providers().len());
    let _ = writeln!(out, "  Agents      {}", app.agents().len());
    let _ = writeln!(out, "  Skills      {}", app.skills().len());
    let _ = writeln!(out, "  Commands    {}", app.commands().len());
    let plugins = app.plugins.list();
    let _ = writeln!(out, "  Plugins     {} ({})", plugins.len(), if app.plugins.enabled() { "enabled" } else { "disabled" });
    for error in app.plugins.errors() {
        let _ = writeln!(out, "    error     {error}");
    }
    let names = app.mcp.server_names();
    let _ = writeln!(out, "  MCP servers {} configured", app.mcp.configured());
    if !names.is_empty() {
        let _ = writeln!(out, "    {}", names.join(", "));
    }
    out
}

/// Plugin id, source, features and state.
pub fn plugins(app: &crate::app::App) -> String {
    let mut out = String::new();
    let loaded = app.plugins.list();
    if loaded.is_empty() {
        let _ = writeln!(out, "No plugins loaded.");
        if !app.plugins.enabled() {
            let _ = writeln!(out, "Plugins are disabled for this build.");
        }
    } else {
        for plugin in loaded {
            let id = field(&plugin, "id");
            let source = plugin
                .get("source")
                .map(|source| match source.get("type").and_then(Value::as_str) {
                    Some("local") => field(source, "path"),
                    Some("package") => {
                        let target = field(source, "target");
                        match field(source, "version") {
                            version if version.is_empty() => target,
                            version => format!("{target}@{version}"),
                        }
                    }
                    _ => source.to_string(),
                })
                .unwrap_or_default();
            let state = plugin
                .get("state")
                .and_then(|state| state.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let features = plugin
                .get("features")
                .and_then(Value::as_object)
                .map(|features| {
                    features
                        .iter()
                        .filter(|(_, value)| value.as_bool() == Some(true))
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let _ = writeln!(out, "{id}");
            let _ = writeln!(out, "  source    {source}");
            let _ = writeln!(out, "  state     {state}");
            if !features.is_empty() {
                let _ = writeln!(out, "  features  {features}");
            }
        }
    }
    for error in app.plugins.errors() {
        let _ = writeln!(out, "error  {error}");
    }
    out
}

/// Aggregate session statistics.
pub fn stats() -> String {
    match crate::stats::data(None) {
        Ok(data) => crate::stats::format(&data),
        Err(error) => format!("Statistics are unavailable: {error}\n"),
    }
}

/// Working-tree diff for `directory`.
pub fn diff(app: &crate::app::App, directory: &Path) -> String {
    let _ = app;
    let input = serde_json::json!({
        "directory": directory.to_string_lossy().to_string(),
        "mode": "working",
    });
    match crate::vcs::diff(&input) {
        Ok(Value::Array(items)) if items.is_empty() => "No changes in the working tree.\n".to_string(),
        Ok(Value::Array(items)) => {
            let mut out = String::new();
            for item in items {
                let file = field(&item, "file");
                let status = field(&item, "status");
                let additions = item.get("additions").and_then(Value::as_u64).unwrap_or(0);
                let deletions = item.get("deletions").and_then(Value::as_u64).unwrap_or(0);
                let _ = writeln!(out, "{file}  [{status}] +{additions} -{deletions}");
                let patch = field(&item, "patch");
                if patch.is_empty() {
                    let _ = writeln!(out, "  (no patch available)");
                } else {
                    for line in patch.lines() {
                        let _ = writeln!(out, "{line}");
                    }
                }
                let _ = writeln!(out);
            }
            out
        }
        Ok(other) => format!("Unexpected diff response: {other}\n"),
        Err(error) => format!("Diff unavailable: {error}\n"),
    }
}

/// This build has no bundled release-highlights feed.
pub fn highlights() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Release highlights");
    let _ = writeln!(out);
    let _ = writeln!(out, "This build does not bundle a release-highlights feed.");
    let _ = writeln!(out);
    let _ = writeln!(out, "Version       {}", crate::VERSION);
    let _ = writeln!(out);
    let _ = writeln!(out, "Compatibility");
    let _ = writeln!(out, "  This client keeps its own command surface. Settings, sessions and");
    let _ = writeln!(out, "  credentials written by the upstream client are read where their");
    let _ = writeln!(out, "  formats overlap.");
    out
}

/// This build has no in-app updater.
pub fn update() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Updates");
    let _ = writeln!(out);
    let _ = writeln!(out, "This build has no in-app updater.");
    let _ = writeln!(out);
    let _ = writeln!(out, "Version       {}", crate::VERSION);
    let _ = writeln!(out);
    let _ = writeln!(out, "To update, rebuild from source:");
    let _ = writeln!(out, "  cargo build --release");
    let _ = writeln!(out, "  cargo install --path .");
    let _ = writeln!(out, "Then replace the installed `opencode` binary on your PATH.");
    out
}
