//! Helpers behind the TUI slash commands. Kept independent of `tui::mod` so the
//! UI layer only has to call these and render the returned strings.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// One-shot side answer: uses the session context but never appends to it.
pub async fn btw(
    app: std::sync::Arc<crate::app::App>,
    session_id: String,
    question: String,
) -> Result<String> {
    let question = question.trim();
    if question.is_empty() {
        return Err(anyhow!("btw needs a question"));
    }
    let prompt = format!(
        "The user is asking a quick side question about the conversation so far. \
Answer directly and concisely in markdown from what you already know. \
Do not call any tools and do not take any actions.\n\n{question}"
    );
    let value = crate::plugin::client_call(
        &app,
        "session.generate",
        vec![json!({ "sessionID": session_id, "prompt": prompt })],
    )
    .await?;
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(anyhow!("generate returned no text"));
    }
    Ok(text)
}

/// Create a one-time pairing link, with the same Basic auth as `opencode pair`.
pub async fn pair(url: &str, password: Option<&str>) -> Result<String> {
    use base64::Engine;
    let base = url.trim_end_matches('/');
    let mut request = crate::http::client()
        .post(format!("{base}/api/pair"))
        .header("accept", "application/json");
    if let Some(password) = password.filter(|password| !password.is_empty()) {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        request = request.header("authorization", format!("Basic {token}"));
    }
    let response = crate::http::send(request).await?;
    if !response.status().is_success() {
        return Err(anyhow!("pairing failed: HTTP {}", response.status().as_u16()));
    }
    let value = response.json::<Value>().await?;
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("pairing response had no code"))?;
    Ok(format!("{base}/auth/connect/{code}"))
}

/// Stop and start the detached server on the port and directory it was using.
pub fn restart_service() -> Result<String> {
    let status = crate::service::status();
    if status.get("running").and_then(Value::as_bool) != Some(true) {
        return Ok(service_hint());
    }
    let port = status
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| anyhow!("running service has no port"))?;
    let directory = status
        .get("directory")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("running service has no directory"))?
        .to_string();
    crate::service::stop()?;
    let info = crate::service::start(port, std::path::PathBuf::from(&directory))?;
    Ok(format!(
        "Restarted opencode at {} (PID {})",
        info.get("url").and_then(Value::as_str).unwrap_or_default(),
        info.get("pid").and_then(Value::as_u64).unwrap_or_default()
    ))
}

/// Known providers, grouped by whether they can be used right now. The value is
/// `provider:<id>` so the caller can start the connect flow from the selection.
pub fn provider_choices(app: &crate::app::App) -> Vec<crate::tui::dialog::Opt> {
    let providers = app.providers();
    let available = crate::models::available(&app.config(), &providers);
    let creds = crate::auth::all();
    let mut opts: Vec<crate::tui::dialog::Opt> = providers
        .values()
        .map(|provider| {
            let kind = auth_kind(provider, &creds);
            let connected = kind != "none";
            crate::tui::dialog::Opt {
                title: provider.name.clone(),
                description: provider.id.clone(),
                footer: kind.to_string(),
                category: if connected { "Connected" } else { "Not connected" }.into(),
                search: format!("{} {kind}", provider.id),
                value: format!("provider:{}", provider.id),
                current: available.contains(&provider.id),
            }
        })
        .collect();
    for (id, title) in [
        ("openai", "Sign in with ChatGPT"),
        ("github-copilot", "Sign in with GitHub Copilot"),
        ("opencode", "Sign in with OpenCode Console"),
    ] {
        if providers.contains_key(id) && !creds.contains_key(id) {
            opts.push(crate::tui::dialog::Opt {
                title: title.into(),
                description: id.into(),
                footer: "oauth".into(),
                category: "OAuth".into(),
                search: format!("{} oauth sign in", id),
                value: format!("oauth:{id}"),
                current: false,
            });
        }
    }
    opts.sort_by(|a, b| {
        (a.category != "Connected", a.title.to_lowercase())
            .cmp(&(b.category != "Connected", b.title.to_lowercase()))
    });
    opts
}

fn auth_kind<'a>(
    provider: &crate::models::Provider,
    creds: &'a std::collections::BTreeMap<String, crate::auth::Credential>,
) -> &'a str {
    if let Some(credential) = creds.get(&provider.id) {
        return credential.kind();
    }
    if provider.options.api_key.as_deref().is_some_and(|key| !key.is_empty()) {
        return "api";
    }
    if provider.env.iter().any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty())) {
        return "env";
    }
    "none"
}

pub fn save_provider_key(provider: &str, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("API key is empty"));
    }
    crate::auth::set(
        provider,
        crate::auth::Credential::Api { key: key.to_string(), metadata: None },
    )
}

pub fn service_hint() -> String {
    "No background service is running. Start it with `opencode service start` or run `opencode serve`.".into()
}
