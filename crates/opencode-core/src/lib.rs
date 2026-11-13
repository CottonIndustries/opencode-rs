//! Core runtime for the OpenCode Rust port: configuration, models, providers,
//! sessions, the agent engine, tools, plugins, storage and supporting services.

pub mod account;
pub mod agent;
pub mod app;
pub mod artifact;
pub mod auth;
pub mod command;
pub mod config;
pub mod config_watch;
pub mod console;
pub mod db;
pub mod debug_dump;
pub mod engine;
pub mod event_store;
pub mod formatter;
pub mod forms;
pub mod http;
pub mod job;
pub mod lsp;
pub mod mcp;
pub mod mcp_oauth;
pub mod media;
pub mod mentions;
pub mod migration;
pub mod models;
pub mod paths;
pub mod permission;
pub mod permission_saved;
pub mod plugin;
pub mod plugin_package;
pub mod policy;
pub mod prompt;
pub mod provider;
pub mod repository;
pub mod retention;
pub mod schedule;
pub mod service;
pub mod session;
pub mod share;
pub mod snapshot;
pub mod state;
pub mod stats;
pub mod tools;
pub mod util;
pub mod vcs;
pub mod voice;
pub mod wellknown;
pub mod workspace;

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
