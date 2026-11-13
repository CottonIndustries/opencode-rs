//! Provider OAuth logins and request-time token refresh.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const COPILOT_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const CONSOLE_CLIENT_ID: &str = "opencode-cli";
const CONSOLE_SERVER: &str = "https://opencode.ai/console";

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(len: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

async fn post_form(url: &str, form: &[(&str, String)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(key, value)| (*key, value.as_str())))
        .finish();
    let response = crate::http::send(
        crate::http::client()
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", crate::user_agent())
            .body(body),
    )
    .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {status}: {}", crate::util::truncate_chars(&text, 300)));
    }
    Ok(response.json::<Value>().await?)
}

async fn post_json(url: &str, body: &Value, accept: &str) -> Result<Value> {
    let response = crate::http::send(
        crate::http::client()
            .post(url)
            .header("accept", accept)
            .header("content-type", "application/json")
            .header("user-agent", crate::user_agent())
            .json(body),
    )
    .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {status}: {}", crate::util::truncate_chars(&text, 300)));
    }
    Ok(response.json::<Value>().await?)
}

async fn post_json_status(url: &str, body: &Value) -> Result<(u16, Value)> {
    let response = crate::http::send(
        crate::http::client()
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("user-agent", crate::user_agent())
            .json(body),
    )
    .await?;
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, value))
}

async fn get_json_plain(url: &str, authorization: &str) -> Result<Value> {
    let response = crate::http::send(
        crate::http::client()
            .get(url)
            .header("accept", "application/json")
            .header("authorization", authorization)
            .header("user-agent", crate::user_agent()),
    )
    .await?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {}", response.status().as_u16()));
    }
    Ok(response.json::<Value>().await?)
}

async fn get_json(url: &str, authorization: &str) -> Result<Value> {
    let response = crate::http::send(
        crate::http::client()
            .get(url)
            .header("accept", "application/json")
            .header("authorization", authorization)
            .header("user-agent", crate::user_agent())
            .header("x-github-api-version", "2022-11-28"),
    )
    .await?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {}", response.status().as_u16()));
    }
    Ok(response.json::<Value>().await?)
}

fn jwt_claim(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn openai_account_id(tokens: &Value) -> Option<String> {
    for key in ["id_token", "access_token"] {
        let Some(token) = tokens.get(key).and_then(Value::as_str) else { continue };
        let Some(claims) = jwt_claim(token) else { continue };
        if let Some(id) = claims.get("chatgpt_account_id").and_then(Value::as_str) {
            return Some(id.to_string());
        }
        if let Some(id) = claims
            .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
            .and_then(Value::as_str)
        {
            return Some(id.to_string());
        }
        if let Some(id) = claims
            .get("organizations")
            .and_then(Value::as_array)
            .and_then(|organizations| organizations.first())
            .and_then(|organization| organization.get("id"))
            .and_then(Value::as_str)
        {
            return Some(id.to_string());
        }
    }
    None
}

async fn wait_for_callback(listener: TcpListener, state: &str) -> Result<String> {
    let (mut stream, _) = listener.accept().await?;
    let mut buffer = vec![0u8; 16 * 1024];
    let read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let url = url::Url::parse(&format!("http://127.0.0.1{path}"))?;
    let parameters: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let body = if let Some(error) = parameters.get("error") {
        format!("Authorization failed: {error}")
    } else if parameters.get("state").map(String::as_str) != Some(state) {
        "Authorization state mismatch".to_string()
    } else {
        "Authorization complete. You can close this window.".to_string()
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    if parameters.get("error").is_some() {
        return Err(anyhow!("OAuth authorization failed"));
    }
    if parameters.get("state").map(String::as_str) != Some(state) {
        return Err(anyhow!("OAuth state mismatch"));
    }
    parameters
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("OAuth callback had no code"))
}

async fn openai_login() -> Result<()> {
    let mut listener = None;
    for port in OPENAI_CALLBACK_PORTS {
        if let Ok(bound) = TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some((bound, port));
            break;
        }
    }
    let (listener, port) = listener.ok_or_else(|| anyhow!("OpenAI login needs local port 1455 or 1457"))?;
    let redirect = format!("http://localhost:{port}/auth/callback");
    let verifier = base64_url(&random_bytes(64));
    let challenge = base64_url(&Sha256::digest(verifier.as_bytes()));
    let state = base64_url(&random_bytes(16));
    let mut authorize = url::Url::parse(&format!("{OPENAI_ISSUER}/oauth/authorize"))?;
    {
        let mut query = authorize.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", OPENAI_CLIENT_ID);
        query.append_pair("redirect_uri", &redirect);
        query.append_pair("scope", "openid profile email offline_access");
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("id_token_add_organizations", "true");
        query.append_pair("codex_cli_simplified_flow", "true");
        query.append_pair("state", &state);
        query.append_pair("originator", "opencode");
    }
    eprintln!("Open this URL to sign in to ChatGPT:\n{authorize}");
    open_browser(authorize.as_str());
    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_callback(listener, &state))
        .await
        .map_err(|_| anyhow!("OpenAI login timed out"))??;
    let tokens = post_form(
        &format!("{OPENAI_ISSUER}/oauth/token"),
        &[
            ("grant_type", "authorization_code".into()),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", OPENAI_CLIENT_ID.into()),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenAI token response had no access_token"))?
        .to_string();
    let refresh = tokens.get("refresh_token").and_then(Value::as_str).unwrap_or_default().to_string();
    let expires = tokens
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| crate::util::now_ms() + seconds as i64 * 1000)
        .unwrap_or(0);
    crate::auth::set(
        "openai",
        crate::auth::Credential::Oauth {
            access,
            refresh,
            expires,
            account_id: openai_account_id(&tokens),
            enterprise_url: None,
            metadata: Some(json!({
                "methodID": "chatgpt-browser",
                "refreshURL": format!("{OPENAI_ISSUER}/oauth/token"),
                "clientID": OPENAI_CLIENT_ID,
            })),
        },
    )?;
    eprintln!("Signed in to ChatGPT");
    Ok(())
}

async fn copilot_login() -> Result<()> {
    let device = post_json(
        "https://github.com/login/device/code",
        &json!({ "client_id": COPILOT_CLIENT_ID, "scope": "read:user" }),
        "application/json",
    )
    .await?;
    let code = device
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub device response had no user_code"))?;
    let verification = device
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://github.com/login/device");
    eprintln!("Open {verification} and enter code: {code}");
    open_browser(verification);
    let mut interval = device.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1);
    let device_code = device
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub device response had no device_code"))?
        .to_string();
    let access = loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let token = post_json(
            "https://github.com/login/oauth/access_token",
            &json!({
                "client_id": COPILOT_CLIENT_ID,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }),
            "application/json",
        )
        .await?;
        if let Some(access) = token.get("access_token").and_then(Value::as_str) {
            break access.to_string();
        }
        match token.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(error) => return Err(anyhow!("GitHub device authorization failed: {error}")),
            None => return Err(anyhow!("GitHub device authorization failed")),
        }
    };
    let user = get_json("https://api.github.com/user", &format!("Bearer {access}"))
        .await
        .ok();
    let endpoint = user
        .as_ref()
        .and_then(|user| user.pointer("/endpoints/api"))
        .and_then(Value::as_str)
        .unwrap_or("https://api.githubcopilot.com")
        .trim_end_matches('/')
        .to_string();
    crate::auth::set(
        "github-copilot",
        crate::auth::Credential::Oauth {
            access: access.clone(),
            refresh: access,
            expires: 0,
            account_id: user
                .as_ref()
                .and_then(|user| user.get("login"))
                .and_then(Value::as_str)
                .map(String::from),
            enterprise_url: None,
            metadata: Some(json!({
                "methodID": "device",
                "apiEndpoint": endpoint,
            })),
        },
    )?;
    eprintln!("Signed in to GitHub Copilot");
    Ok(())
}

fn normalize_console_server(input: Option<&str>) -> Result<String> {
    let raw = input.unwrap_or(CONSOLE_SERVER);
    let url = url::Url::parse(raw).map_err(|error| anyhow!("Invalid OpenCode server URL: {error}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(anyhow!("Invalid OpenCode server URL: expected HTTP(S)"));
    }
    Ok(format!(
        "{}://{}{}",
        url.scheme(),
        url.authority(),
        url.path().trim_end_matches('/')
    ))
}

fn console_token(value: &Value) -> Option<(
    String,
    String,
    u64,
    Option<String>,
)> {
    Some((
        value.get("access_token").and_then(Value::as_str)?.to_string(),
        value.get("refresh_token").and_then(Value::as_str)?.to_string(),
        value.get("expires_in").and_then(Value::as_u64).unwrap_or(0),
        value.get("org_id").and_then(Value::as_str).map(String::from),
    ))
}

async fn console_login(server: Option<&str>) -> Result<()> {
    let server = normalize_console_server(server)?;
    let (status, device) = post_json_status(
        &format!("{server}/auth/device/code"),
        &json!({ "client_id": CONSOLE_CLIENT_ID, "supports_org_scope": true }),
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!(
            "OpenCode device authorization failed: HTTP {status}: {}",
            crate::util::truncate_chars(&device.to_string(), 300)
        ));
    }
    let user_code = device
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenCode device response had no user_code"))?;
    let verification = device
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenCode device response had no verification_uri_complete"))?;
    let verification = match url::Url::parse(verification) {
        Ok(url) => url,
        Err(_) => url::Url::parse(&format!("{server}/"))
            .and_then(|base| base.join(verification))
            .map_err(|error| anyhow!("Invalid device verification URL: {error}"))?,
    };
    if verification.scheme() != "http" && verification.scheme() != "https" {
        return Err(anyhow!("Invalid device verification URL: expected HTTP(S)"));
    }
    let verification = verification.to_string();
    let device_code = device
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenCode device response had no device_code"))?
        .to_string();
    let expires_in = device.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
    eprintln!("Open {verification} and enter code: {user_code}");
    open_browser(&verification);
    let mut interval = device.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1);
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in.max(1));
    let token = loop {
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("OpenCode device authorization timed out"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let (status, value) = post_json_status(
            &format!("{server}/auth/device/token"),
            &json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device_code,
                "client_id": CONSOLE_CLIENT_ID,
            }),
        )
        .await?;
        if let Some(token) = console_token(&value) {
            break token;
        }
        match value.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(error) => return Err(anyhow!("OpenCode device authorization failed: {error} (HTTP {status})")),
            None => return Err(anyhow!("OpenCode device authorization failed: HTTP {status}")),
        }
    };
    let (access, refresh, expires_in, org_id) = token;
    let bearer = format!("Bearer {access}");
    let user_url = format!("{server}/api/user");
    let orgs_url = format!("{server}/api/orgs");
    let (user, orgs) = tokio::join!(
        get_json_plain(&user_url, &bearer),
        get_json_plain(&orgs_url, &bearer),
    );
    let user = user?;
    let mut orgs = orgs?.as_array().cloned().unwrap_or_default();
    orgs.sort_by(|a, b| {
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
        let other = b.get("name").and_then(Value::as_str).unwrap_or_default();
        name.cmp(other).then_with(|| {
            a.get("id").and_then(Value::as_str).unwrap_or_default().cmp(
                b.get("id").and_then(Value::as_str).unwrap_or_default(),
            )
        })
    });
    let org = match org_id.as_deref() {
        Some(org_id) => Some(
            orgs.iter()
                .find(|org| org.get("id").and_then(Value::as_str) == Some(org_id))
                .cloned()
                .ok_or_else(|| anyhow!("OpenCode organization not found: {org_id}"))?,
        ),
        None => orgs.first().cloned(),
    };
    let user_id = user.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let email = user.get("email").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut metadata = serde_json::Map::new();
    metadata.insert("methodID".into(), json!("device"));
    metadata.insert("server".into(), json!(server));
    metadata.insert("accountID".into(), json!(user_id));
    metadata.insert("email".into(), json!(email));
    metadata.insert("refreshURL".into(), json!(format!("{server}/auth/device/token")));
    metadata.insert("clientID".into(), json!(CONSOLE_CLIENT_ID));
    if let Some(org) = org {
        if let Some(id) = org.get("id").and_then(Value::as_str) {
            metadata.insert("orgID".into(), json!(id));
        }
        if let Some(name) = org.get("name").and_then(Value::as_str) {
            metadata.insert("orgName".into(), json!(name));
        }
    }
    crate::auth::set(
        "opencode",
        crate::auth::Credential::Oauth {
            access,
            refresh,
            expires: crate::util::now_ms() + expires_in as i64 * 1000,
            account_id: Some(user_id),
            enterprise_url: None,
            metadata: Some(Value::Object(metadata)),
        },
    )?;
    eprintln!("Signed in to OpenCode Console as {email}");
    match crate::console::sync().await {
        Ok(summary) => eprintln!(
            "Synced Console workspace: {} providers, {} instructions, {} skills",
            summary["providers"], summary["instructions"], summary["skills"]
        ),
        Err(error) => eprintln!("Signed in, but Console workspace sync failed: {error}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chatgpt_account_from_a_jwt() {
        let payload = base64_url(br#"{"chatgpt_account_id":"acct_1"}"#);
        let token = format!("header.{payload}.signature");
        let tokens = json!({ "access_token": token });
        assert_eq!(openai_account_id(&tokens).as_deref(), Some("acct_1"));
    }

    #[test]
    fn ignores_a_non_jwt_token() {
        assert!(openai_account_id(&json!({ "access_token": "opaque" })).is_none());
    }

    #[test]
    fn normalizes_console_servers() {
        assert_eq!(normalize_console_server(None).unwrap(), CONSOLE_SERVER);
        assert_eq!(normalize_console_server(Some("https://example.com/console/")).unwrap(), "https://example.com/console");
        assert_eq!(normalize_console_server(Some("http://127.0.0.1:4096")).unwrap(), "http://127.0.0.1:4096");
        assert!(normalize_console_server(Some("file:///tmp/console")).is_err());
    }

    #[test]
    fn reads_console_device_tokens() {
        let token = console_token(&json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 600,
            "org_id": "org_1"
        }))
        .unwrap();
        assert_eq!(token.0, "access");
        assert_eq!(token.1, "refresh");
        assert_eq!(token.2, 600);
        assert_eq!(token.3.as_deref(), Some("org_1"));
        assert!(console_token(&json!({ "error": "authorization_pending" })).is_none());
    }
}

pub async fn login(provider: &str, server: Option<&str>) -> Result<()> {
    match provider {
        "openai" | "chatgpt" => openai_login().await,
        "github-copilot" | "copilot" => copilot_login().await,
        "opencode" | "opencode-console" | "console" => console_login(server).await,
        other => Err(anyhow!("{other} has no OAuth login; pass --key to store an API key")),
    }
}

/// Apply provider settings derived from an OAuth credential.
pub fn apply(provider: &mut crate::models::Provider) {
    let Some(crate::auth::Credential::Oauth { metadata, .. }) = crate::auth::get(&provider.id) else {
        return;
    };
    let metadata = metadata.unwrap_or(Value::Null);
    match provider.id.as_str() {
        "openai" => {
            provider.options.base_url = Some("https://chatgpt.com/backend-api/codex".into());
            provider.options.extra.insert("transport".into(), json!("websocket"));
            provider.options.headers.insert("originator".into(), "opencode".into());
            provider.options.headers.insert("x-codex-beta-features".into(), "remote_compaction_v2".into());
            if let Some(account) = metadata.get("accountID").and_then(Value::as_str) {
                provider.options.headers.insert("chatgpt-account-id".into(), account.to_string());
            }
        }
        "github-copilot" => {
            if let Some(endpoint) = metadata.get("apiEndpoint").and_then(Value::as_str) {
                provider.options.base_url = Some(endpoint.trim_end_matches('/').to_string());
            }
            provider.options.headers.insert("copilot-integration-id".into(), "vscode-chat".into());
            provider.options.headers.insert("editor-version".into(), format!("opencode/{}", crate::VERSION));
        }
        _ => {}
    }
}

/// Refresh an expired OAuth token before a request.
pub async fn refresh(provider: &mut crate::models::Provider) {
    let Some(crate::auth::Credential::Oauth { access, refresh, expires, account_id, enterprise_url, metadata }) =
        crate::auth::get(&provider.id)
    else {
        return;
    };
    if expires == 0 || expires > crate::util::now_ms() + 60_000 {
        return;
    }
    let mut metadata = metadata.unwrap_or(Value::Null);
    let (Some(url), Some(client_id)) = (
        metadata.get("refreshURL").and_then(Value::as_str),
        metadata.get("clientID").and_then(Value::as_str),
    ) else {
        return;
    };
    if refresh.is_empty() {
        return;
    }
    let tokens = if provider.id == "opencode" {
        post_json(
            url,
            &json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh,
                "client_id": client_id,
            }),
            "application/json",
        )
        .await
    } else {
        let form = [
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh.clone()),
            ("client_id", client_id.to_string()),
        ];
        post_form(url, &form).await
    };
    let Ok(tokens) = tokens else { return };
    let Some(next) = tokens.get("access_token").and_then(Value::as_str) else { return };
    let next_refresh = tokens.get("refresh_token").and_then(Value::as_str).unwrap_or(&refresh).to_string();
    let next_expires = tokens
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| crate::util::now_ms() + seconds as i64 * 1000)
        .unwrap_or(0);
    if provider.id == "opencode" {
        if !metadata.is_object() {
            metadata = json!({});
        }
        if let Some(org) = tokens.get("org_id").and_then(Value::as_str) {
            let same = metadata.get("orgID").and_then(Value::as_str) == Some(org);
            metadata["orgID"] = json!(org);
            if !same || metadata.get("orgName").and_then(Value::as_str).is_none() {
                metadata["orgName"] = json!(org);
            }
        }
    }
    if crate::auth::set(
        &provider.id,
        crate::auth::Credential::Oauth {
            access: next.to_string(),
            refresh: next_refresh,
            expires: next_expires,
            account_id,
            enterprise_url,
            metadata: Some(metadata),
        },
    )
    .is_ok()
    {
        provider.key = Some(next.to_string());
    }
}
