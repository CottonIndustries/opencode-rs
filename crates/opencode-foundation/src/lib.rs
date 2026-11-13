//! Low-level filesystem, HTTP, credentials and shared utility support.

pub mod account;
pub mod auth;
pub mod http;
pub mod paths;
pub mod state;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Upstream release this port mirrors; reported to providers like the TypeScript client does.
pub const UPSTREAM_VERSION: &str = "2.2.7";
pub const UPSTREAM_CHANNEL: &str = "latest";

/// Client name reported to providers (`x-opencode-client`): `OPENCODE_CLIENT` or `cli`, as upstream.
pub fn client_name() -> String {
    std::env::var("OPENCODE_CLIENT").ok().filter(|value| !value.is_empty()).unwrap_or_else(|| "cli".into())
}

/// `opencode/<channel>/<version>/<client>`, the TypeScript client's User-Agent.
pub fn user_agent() -> String {
    format!("opencode/{UPSTREAM_CHANNEL}/{UPSTREAM_VERSION}/{}", client_name())
}
