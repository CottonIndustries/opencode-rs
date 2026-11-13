//! MCP OAuth: discovery, dynamic client registration, PKCE authorization,
//! token storage and refresh.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::config::McpConfig;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes: Vec<String>,
}

#[derive(Clone)]
struct Tokens {
    access: String,
    refresh: String,
    expires: i64,
    token_endpoint: String,
    client_id: String,
    client_secret: String,
    resource: String,
    scope: String,
    issuer: String,
}

/// A started authorization flow. begin() performs discovery and client registration; finish()
/// waits for the browser callback and exchanges the code, so a UI can show the URL first.
pub struct SignIn {
    server: String,
    pub url: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect: String,
    client_id: String,
    client_secret: String,
    discovery: Discovery,
    resource: String,
    scope: String,
    paste_tx: mpsc::UnboundedSender<Result<String, String>>,
    paste_rx: mpsc::UnboundedReceiver<Result<String, String>>,
}

/// Lets a UI submit the redirect URL when the browser cannot reach the local callback listener.
#[derive(Clone)]
pub struct SignInHandle {
    state: String,
    paste_tx: mpsc::UnboundedSender<Result<String, String>>,
}

impl std::fmt::Debug for SignInHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignInHandle").field("state", &self.state).finish()
    }
}

impl SignIn {
    pub fn handle(&self) -> SignInHandle {
        SignInHandle { state: self.state.clone(), paste_tx: self.paste_tx.clone() }
    }
}

impl SignInHandle {
    pub fn cancel(&self) {
        let _ = self.paste_tx.send(Err("OAuth authorization cancelled".into()));
    }

    /// Validate a pasted redirect URL and hand its code to the waiting finish() call.
    pub fn submit(&self, pasted: &str) -> Result<()> {
        let text = pasted.trim();
        let url = url::Url::parse(text).map_err(|_| anyhow!("Paste the full URL from the browser's address bar"))?;
        let parameters: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        if parameters.get("state").map(String::as_str) != Some(self.state.as_str()) {
            return Err(anyhow!("This URL belongs to a different sign-in"));
        }
        if let Some(error) = parameters.get("error_description").or_else(|| parameters.get("error")) {
            let _ = self.paste_tx.send(Err(error.clone()));
            return Ok(());
        }
        let Some(code) = parameters.get("code") else {
            return Err(anyhow!("The URL does not contain an authorization code"));
        };
        self.paste_tx.send(Ok(code.clone())).map_err(|_| anyhow!("Sign-in attempt ended"))?;
        Ok(())
    }
}

fn credential_key(server: &str) -> String {
    format!("mcp:{server}")
}

fn load(server: &str) -> Option<Tokens> {
    let credential = crate::auth::get(&credential_key(server))?;
    let crate::auth::Credential::Oauth { access, refresh, expires, metadata, .. } = credential else {
        return None;
    };
    let metadata = metadata?;
    let text = |key: &str| metadata.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    Some(Tokens {
        access,
        refresh,
        expires,
        token_endpoint: text("tokenEndpoint"),
        client_id: text("clientID"),
        client_secret: text("clientSecret"),
        resource: text("resource"),
        scope: text("scope"),
        issuer: text("issuer"),
    })
}

fn save(server: &str, tokens: &Tokens) -> Result<()> {
    crate::auth::set(
        &credential_key(server),
        crate::auth::Credential::Oauth {
            access: tokens.access.clone(),
            refresh: tokens.refresh.clone(),
            expires: tokens.expires,
            account_id: None,
            enterprise_url: None,
            metadata: Some(json!({
                "tokenEndpoint": tokens.token_endpoint,
                "clientID": tokens.client_id,
                "clientSecret": tokens.client_secret,
                "resource": tokens.resource,
                "scope": tokens.scope,
                "issuer": tokens.issuer,
            })),
        },
    )
}

pub fn logout(server: &str) -> Result<bool> {
    crate::auth::remove(&credential_key(server))
}

pub fn logged_in(server: &str) -> bool {
    load(server).is_some()
}

fn configured_oauth(config: &McpConfig) -> Option<&Value> {
    config.oauth.as_ref().filter(|value| value.as_object().is_some())
}

fn string_field(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

/// OAuth options use camelCase upstream and snake_case in older configs.
fn oauth_string(oauth: &Value, key: &str, legacy: &str) -> Option<String> {
    string_field(Some(oauth), key).or_else(|| string_field(Some(oauth), legacy))
}

fn oauth_port(oauth: &Value, key: &str, legacy: &str) -> Option<u16> {
    let value = oauth.get(key).or_else(|| oauth.get(legacy))?;
    value
        .as_u64()
        .map(|port| port as u16)
        .or_else(|| value.as_str().and_then(|port| port.parse::<u16>().ok()))
}

async fn get_json(url: &str, headers: &[(String, String)]) -> Result<Value> {
    let mut builder = crate::http::client().get(url);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = crate::http::send(builder).await?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {} for {url}", response.status().as_u16()));
    }
    Ok(response.json::<Value>().await?)
}

async fn post_json(url: &str, body: &Value) -> Result<Value> {
    let response = crate::http::send(crate::http::client().post(url).json(body)).await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {status} for {url}: {}", crate::util::truncate_chars(&text, 300)));
    }
    Ok(response.json::<Value>().await?)
}

async fn post_form(url: &str, form: &[(&str, String)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(key, value)| (*key, value.as_str())))
        .finish();
    let response = crate::http::send(
        crate::http::client()
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body),
    )
    .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {status} for {url}: {}", crate::util::truncate_chars(&text, 300)));
    }
    Ok(response.json::<Value>().await?)
}

async fn discover(config: &McpConfig) -> Result<Discovery> {
    let oauth = configured_oauth(config).cloned().unwrap_or(Value::Null);
    if let Some(url) = oauth_string(&oauth, "authServerMetadataUrl", "auth_server_metadata_url") {
        let metadata = get_json(&url, &[]).await?;
        return discovery_from_metadata(&metadata);
    }
    let server = config.url.as_deref().ok_or_else(|| anyhow!("remote MCP server has no url"))?;
    let url = url::Url::parse(server)?;
    let mut resource_metadata_url = oauth_string(&oauth, "resourceMetadataUrl", "resource_metadata_url");
    if resource_metadata_url.is_none() {
        let response = crate::http::send(crate::http::client().get(server)).await;
        if let Ok(response) = response {
            if let Some(header) = response.headers().get("www-authenticate").and_then(|value| value.to_str().ok()) {
                resource_metadata_url = header.split(',').find_map(|part| {
                    let part = part.trim();
                    part.strip_prefix("resource_metadata=\"")
                        .and_then(|rest| rest.strip_suffix('"'))
                        .map(String::from)
                });
            }
        }
    }
    let resource_metadata_url = match resource_metadata_url {
        Some(url) => url,
        None => {
            let base = format!("{}://{}", url.scheme(), url.authority());
            let path = url.path().trim_end_matches('/');
            let candidate = format!("{base}/.well-known/oauth-protected-resource{path}");
            if get_json(&candidate, &[]).await.is_ok() {
                candidate
            } else {
                format!("{base}/.well-known/oauth-protected-resource")
            }
        }
    };
    let resource_metadata = get_json(&resource_metadata_url, &[]).await?;
    let issuer = resource_metadata
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| oauth_string(&oauth, "issuer", "issuer"))
        .unwrap_or_else(|| {
            let mut issuer = url.clone();
            issuer.set_path("");
            issuer.set_query(None);
            issuer.set_fragment(None);
            issuer.to_string().trim_end_matches('/').to_string()
        });
    let mut metadata_url = format!("{}/.well-known/oauth-authorization-server", issuer.trim_end_matches('/'));
    let metadata = match get_json(&metadata_url, &[]).await {
        Ok(metadata) => metadata,
        Err(_) => {
            metadata_url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
            get_json(&metadata_url, &[]).await?
        }
    };
    let mut discovery = discovery_from_metadata(&metadata)?;
    discovery.issuer = issuer;
    // The resource metadata names the scopes the MCP server actually accepts, which is narrower
    // than the authorization server's full catalogue (Context7 asks for profile/email, not
    // openid and offline_access).
    if let Some(scopes) = resource_metadata.get("scopes_supported").and_then(Value::as_array) {
        let scopes: Vec<String> = scopes.iter().filter_map(Value::as_str).map(String::from).collect();
        if !scopes.is_empty() {
            discovery.scopes = scopes;
        }
    }
    Ok(discovery)
}

fn discovery_from_metadata(metadata: &Value) -> Result<Discovery> {
    let endpoint = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .ok_or_else(|| anyhow!("OAuth metadata has no {key}"))
    };
    Ok(Discovery {
        issuer: metadata
            .get("issuer")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        authorization_endpoint: endpoint("authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
        registration_endpoint: metadata
            .get("registration_endpoint")
            .and_then(Value::as_str)
            .map(String::from),
        scopes: metadata
            .get("scopes_supported")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default(),
    })
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(len: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

pub fn open_browser(url: &str) -> Result<()> {
    if std::env::var("OPENCODE_MCP_NO_BROWSER").is_ok() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x0800_0000)
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// Start authorization: discovery, client registration, PKCE and the callback listener.
/// The caller presents the URL and then calls finish().
pub async fn begin(server: &str, config: &McpConfig) -> Result<SignIn> {
    let oauth = configured_oauth(config).cloned().unwrap_or(Value::Null);
    let discovery = discover(config).await?;
    let resource = config
        .url
        .clone()
        .ok_or_else(|| anyhow!("remote MCP server has no url"))?;
    let configured_redirect = oauth_string(&oauth, "redirectUri", "redirect_uri");
    let bind_port = configured_redirect
        .as_deref()
        .and_then(|value| url::Url::parse(value).ok())
        .and_then(|url| url.port())
        .or_else(|| oauth_port(&oauth, "callbackPort", "callback_port"))
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", bind_port)).await?;
    let port = listener.local_addr()?.port();
    let redirect = configured_redirect.unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
    let (client_id, client_secret) =
        match (oauth_string(&oauth, "clientId", "client_id"), oauth_string(&oauth, "clientSecret", "client_secret")) {
            (Some(id), secret) => (id, secret.unwrap_or_default()),
            _ => {
                let endpoint = discovery
                    .registration_endpoint
                    .as_deref()
                    .ok_or_else(|| anyhow!("OAuth server supports neither a configured client_id nor registration"))?;
                let registered = post_json(
                    endpoint,
                    &json!({
                        "client_name": "opencode",
                        "client_uri": "https://opencode.ai",
                        "redirect_uris": [redirect],
                        "grant_types": ["authorization_code", "refresh_token"],
                        "response_types": ["code"],
                        "token_endpoint_auth_method": "none",
                    }),
                )
                .await?;
                let id = registered
                    .get("client_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("OAuth registration returned no client_id"))?
                    .to_string();
                (
                    id,
                    registered.get("client_secret").and_then(Value::as_str).unwrap_or_default().to_string(),
                )
            }
        };
    let verifier = base64_url(&random_bytes(32));
    let challenge = base64_url(&Sha256::digest(verifier.as_bytes()));
    let state = base64_url(&random_bytes(16));
    let (paste_tx, paste_rx) = mpsc::unbounded_channel();
    let scope = oauth_string(&oauth, "scope", "scope")
        .or_else(|| (!discovery.scopes.is_empty()).then(|| discovery.scopes.join(" ")))
        .unwrap_or_default();
    let mut authorization = url::Url::parse(&discovery.authorization_endpoint)?;
    {
        let mut query = authorization.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &client_id);
        query.append_pair("redirect_uri", &redirect);
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", &state);
        query.append_pair("resource", &resource);
        if !scope.is_empty() {
            query.append_pair("scope", &scope);
        }
    }
    // The caller presents the URL; the CLI and TUI decide how to open it.
    Ok(SignIn {
        server: server.to_string(),
        url: authorization.to_string(),
        listener,
        state,
        verifier,
        redirect,
        client_id,
        client_secret,
        discovery,
        resource,
        scope,
        paste_tx,
        paste_rx,
    })
}

/// Wait for the browser callback and exchange the code for tokens.
pub async fn finish(sign_in: SignIn) -> Result<()> {
    let SignIn {
        server,
        listener,
        state,
        verifier,
        redirect,
        client_id,
        client_secret,
        discovery,
        resource,
        scope,
        mut paste_rx,
        ..
    } = sign_in;
    let code = tokio::time::timeout(CALLBACK_TIMEOUT, async {
        tokio::select! {
            result = wait_for_code(listener, &state) => result,
            pasted = paste_rx.recv() => match pasted {
                Some(Ok(code)) => Ok(code),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("OAuth authorization cancelled")),
            },
        }
    })
    .await;
    let code = match code {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(anyhow!("OAuth authorization timed out")),
    };
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
        ("resource", resource.clone()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.clone()));
    }
    let tokens = post_form(&discovery.token_endpoint, &form).await?;
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OAuth token response has no access_token"))?
        .to_string();
    let refresh = tokens.get("refresh_token").and_then(Value::as_str).unwrap_or_default().to_string();
    let expires = tokens
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| crate::util::now_ms() + seconds as i64 * 1000)
        .unwrap_or(0);
    save(
        &server,
        &Tokens {
            access,
            refresh,
            expires,
            token_endpoint: discovery.token_endpoint.clone(),
            client_id,
            client_secret,
            resource,
            scope,
            issuer: discovery.issuer,
        },
    )?;
    eprintln!("MCP server {server} authorized");
    Ok(())
}

/// Run the interactive authorization flow and persist the tokens.
pub async fn authorize(server: &str, config: &McpConfig) -> Result<()> {
    let sign_in = begin(server, config).await?;
    eprintln!("Open this URL to authorize MCP server {server}:");
    eprintln!("{}", sign_in.url);
    let _ = open_browser(&sign_in.url);
    finish(sign_in).await
}

async fn wait_for_code(listener: TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0u8; 8192];
        let read = stream.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let url = url::Url::parse(&format!("http://127.0.0.1{path}"))?;
        let parameters: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        if parameters.get("error").is_some() {
            let body = format!("OAuth error: {}", parameters.get("error_description").cloned().unwrap_or_default());
            respond(&mut stream, &body).await?;
            return Err(anyhow!("{body}"));
        }
        let Some(code) = parameters.get("code") else {
            respond(&mut stream, "Waiting for OAuth callback...").await?;
            continue;
        };
        if parameters.get("state").map(String::as_str) != Some(state) {
            respond(&mut stream, "OAuth state mismatch").await?;
            return Err(anyhow!("OAuth state mismatch"));
        }
        respond(&mut stream, "Authorization complete. You can close this window.").await?;
        return Ok(code.clone());
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, message: &str) -> Result<()> {
    let body = format!("<!doctype html><html><body><p>{message}</p></body></html>");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_endpoints_from_metadata() {
        let discovery = discovery_from_metadata(&json!({
            "issuer": "https://auth.example",
            "authorization_endpoint": "https://auth.example/authorize",
            "token_endpoint": "https://auth.example/token",
            "registration_endpoint": "https://auth.example/register",
            "scopes_supported": ["mcp", "offline_access"],
        }))
        .unwrap();
        assert_eq!(discovery.authorization_endpoint, "https://auth.example/authorize");
        assert_eq!(discovery.scopes, vec!["mcp", "offline_access"]);
        assert_eq!(discovery.registration_endpoint.as_deref(), Some("https://auth.example/register"));
    }

    #[test]
    fn pkce_challenge_is_unpadded_base64url() {
        let challenge = base64_url(&Sha256::digest(b"verifier"));
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[tokio::test]
    async fn pasted_redirect_is_validated_and_forwarded() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = SignInHandle { state: "abc".into(), paste_tx: tx };
        assert!(handle.submit("http://127.0.0.1/callback?state=abc&code=xyz").is_ok());
        assert_eq!(rx.recv().await, Some(Ok("xyz".to_string())));
        assert!(handle.submit("http://127.0.0.1/callback?state=other&code=xyz").is_err());
        assert!(handle.submit("http://127.0.0.1/callback?state=abc").is_err());
    }

    #[test]
    fn oauth_false_has_no_configuration() {
        let config = McpConfig {
            kind: Some("remote".into()),
            command: vec![],
            environment: Default::default(),
            cwd: None,
            codemode: None,
            url: Some("https://example.com/mcp".into()),
            headers: Default::default(),
            oauth: Some(Value::Bool(false)),
            protocol: None,
            pool: None,
            enabled: None,
            timeout: None,
        };
        assert!(configured_oauth(&config).is_none());
    }
}

/// Current bearer token, refreshing it when it is close to expiry.
pub async fn access_token(server: &str, _config: &McpConfig) -> Result<Option<String>> {
    let Some(mut tokens) = load(server) else { return Ok(None) };
    let stale = tokens.expires > 0 && tokens.expires <= crate::util::now_ms() + 60_000;
    if stale && !tokens.refresh.is_empty() && !tokens.token_endpoint.is_empty() {
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", tokens.refresh.clone()),
            ("client_id", tokens.client_id.clone()),
        ];
        if !tokens.client_secret.is_empty() {
            form.push(("client_secret", tokens.client_secret.clone()));
        }
        if !tokens.resource.is_empty() {
            form.push(("resource", tokens.resource.clone()));
        }
        let refreshed = post_form(&tokens.token_endpoint, &form).await?;
        if let Some(access) = refreshed.get("access_token").and_then(Value::as_str) {
            tokens.access = access.to_string();
        }
        if let Some(refresh) = refreshed.get("refresh_token").and_then(Value::as_str) {
            tokens.refresh = refresh.to_string();
        }
        if let Some(seconds) = refreshed.get("expires_in").and_then(Value::as_u64) {
            tokens.expires = crate::util::now_ms() + seconds as i64 * 1000;
        }
        save(server, &tokens)?;
    }
    Ok(Some(tokens.access))
}
