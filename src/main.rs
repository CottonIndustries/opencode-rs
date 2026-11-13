//! OpenCode — AI coding agent for the terminal (Rust rewrite).

mod acp;
mod agent;
mod app;
mod artifact;
mod auth;
mod cli;
mod command;
mod config;
mod db;
mod engine;
mod event_store;
mod formatter;
mod forms;
mod http;
mod job;
mod lsp;
mod mcp;
mod mcp_oauth;
mod media;
mod models;
mod paths;
mod permission;
mod plugin;
mod prompt;
mod provider;
mod run;
mod schedule;
mod server;
mod session;
mod snapshot;
mod state;
mod tools;
mod tui;
mod util;
mod vcs;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Upstream release this port mirrors; reported to providers like the TypeScript client does.
pub const UPSTREAM_VERSION: &str = "2.2.7";
pub const UPSTREAM_CHANNEL: &str = "latest";

/// Client name reported to providers (`x-opencode-client`): `OPENCODE_CLIENT` or `cli`, as upstream.
pub fn client_name() -> String {
    std::env::var("OPENCODE_CLIENT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cli".into())
}

/// `opencode/<channel>/<version>/<client>`, the TypeScript client's User-Agent.
pub fn user_agent() -> String {
    format!("opencode/{UPSTREAM_CHANNEL}/{UPSTREAM_VERSION}/{}", client_name())
}

#[derive(Parser)]
#[command(name = "opencode", version = VERSION, about = "OpenCode - AI coding agent for the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Project directory to open
    project: Option<PathBuf>,
    #[command(flatten)]
    tui: TuiArgs,
    /// Print logs to stderr
    #[arg(long, global = true)]
    print_logs: bool,
}

#[derive(clap::Args, Clone, Default)]
struct TuiArgs {
    /// Model to use in the format provider/model
    #[arg(short, long)]
    model: Option<String>,
    /// Agent to use
    #[arg(long)]
    agent: Option<String>,
    /// Continue the last session
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Session ID to continue
    #[arg(short, long)]
    session: Option<String>,
    /// Fork the session when continuing
    #[arg(long)]
    fork: bool,
    /// Prompt to submit on start
    #[arg(long)]
    prompt: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run OpenCode with a message
    Run {
        /// Message to send
        message: Vec<String>,
        /// Continue the last session
        #[arg(short = 'c', long = "continue")]
        continue_last: bool,
        /// Session ID to continue, or to create if it does not exist
        #[arg(short, long)]
        session: Option<String>,
        /// Fork the session before continuing
        #[arg(long)]
        fork: bool,
        /// Model to use in the format provider/model#variant
        #[arg(short, long)]
        model: Option<String>,
        /// Agent to use
        #[arg(long)]
        agent: Option<String>,
        /// Output format
        #[arg(long, default_value = "default", value_parser = ["default", "json"])]
        format: String,
        /// File to attach to the message
        #[arg(short, long)]
        file: Vec<String>,
        /// Session title
        #[arg(long)]
        title: Option<String>,
        /// Show thinking blocks
        #[arg(long)]
        thinking: bool,
        /// Approve permission requests that would otherwise ask
        #[arg(long, alias = "yes", alias = "dangerously-skip-permissions")]
        auto_approve: bool,
        /// Working directory
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Start an Agent Client Protocol server
    Acp {
        /// Run auth login instead of starting the server
        #[arg(long)]
        login: bool,
    },
    /// Create a single-use pairing link for a running server
    Pair {
        /// Server base URL
        #[arg(long, default_value = "http://127.0.0.1:4096")]
        url: String,
        /// Server password (or OPENCODE_SERVER_PASSWORD)
        #[arg(long, env = "OPENCODE_SERVER_PASSWORD")]
        password: Option<String>,
    },
    /// Start the local HTTP API and event feed
    Serve {
        /// Interface to bind
        #[arg(long, default_value = "127.0.0.1")]
        hostname: String,
        /// Port to bind
        #[arg(short, long, default_value_t = 4096)]
        port: u16,
        /// Require Basic authentication with this password
        #[arg(long, env = "OPENCODE_SERVER_PASSWORD")]
        password: Option<String>,
        /// Working directory
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Start the minimal interactive interface
    Mini {
        project: Option<PathBuf>,
        #[command(flatten)]
        tui: TuiArgs,
    },
    /// List all available models
    Models {
        /// Only show models from this provider
        provider: Option<String>,
        /// Show model details
        #[arg(long)]
        verbose: bool,
        /// Refresh the models.dev catalog
        #[arg(long)]
        refresh: bool,
    },
    /// Manage credentials
    Auth {
        #[command(subcommand)]
        command: AuthCmd,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        command: SessionCmd,
    },
    /// Show usage statistics
    Stats {
        /// Show the last N days; 0 means today
        #[arg(long)]
        days: Option<i64>,
        /// Output statistics as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCmd,
    },
    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        command: PluginCmd,
    },
    /// Debugging and troubleshooting
    Debug {
        #[command(subcommand)]
        command: DebugCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// List stored credentials and detected environment keys
    #[command(alias = "ls")]
    List,
    /// Store an API key for a provider
    Login {
        provider: Option<String>,
        /// API key (prompted when omitted)
        #[arg(long)]
        key: Option<String>,
    },
    /// Remove stored credentials
    Logout { provider: Option<String> },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List top-level sessions in the current project, newest first
    #[command(alias = "ls")]
    List {
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        format: String,
    },
    /// Delete a session and its child sessions
    Delete { session_id: String },
    /// Export session data as JSON
    Export {
        session: Option<String>,
        /// Redact sensitive transcript and file data
        #[arg(long)]
        sanitize: bool,
    },
    /// Import session data from a JSON file or URL
    Import { file: String },
}

#[derive(Subcommand)]
enum McpCmd {
    /// List MCP servers and their status
    #[command(alias = "ls")]
    List,
    /// Add an MCP server to configuration
    Add {
        name: String,
        #[arg(long)]
        url: Option<String>,
        /// HTTP header as name=value
        #[arg(long = "header")]
        header: Vec<String>,
        /// Environment variable as name=value
        #[arg(long = "env")]
        env: Vec<String>,
        /// Write to the global configuration instead of the project
        #[arg(long)]
        global: bool,
        /// Local server command and arguments after --
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Authorize a remote MCP server with OAuth
    Auth { name: String },
    /// Forget a remote MCP server's OAuth credentials
    Logout { name: String },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// List configured and discovered plugins
    #[command(alias = "ls")]
    List,
    /// Add a plugin package to the global configuration
    Add { package: String },
    /// Remove a plugin package from the global configuration
    Remove { package: String },
    /// Check configured package plugins for updates
    Check,
    /// Update configured package plugins
    Update,
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Show global paths
    Paths,
    /// Show the resolved configuration
    Config,
    /// List agents
    Agents,
    /// List skills
    Skills,
    /// Print the system prompt for the default agent and model
    Prompt,
    /// List loaded plugins and the tools they contribute
    Plugins,
    /// Execute a plugin tool with JSON input
    PluginTool {
        name: String,
        #[arg(default_value = "{}")]
        input: String,
    },
    /// Execute a plugin TUI command
    PluginTui {
        name: String,
        #[arg(default_value = "")]
        input: String,
    },
    /// Show durable session events
    Events {
        session: Option<String>,
    },
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if cli.print_logs {
        std::env::set_var("OPENCODE_PRINT_LOGS", "1");
    }
    if let Err(e) = dispatch(cli).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // refresh the models catalog in the background (best effort)
    tokio::spawn(async {
        let _ = models::refresh().await;
    });
    match cli.command {
        None => {
            let dir = cli.project.unwrap_or_else(cwd);
            tui::start(dir, cli.tui.into()).await
        }
        Some(Cmd::Acp { login: true }) => cli::auth_login(None, None).await,
        Some(Cmd::Acp { login: false }) => acp::serve().await,
        Some(Cmd::Pair { url, password }) => cli::pair(&url, password).await,
        Some(Cmd::Serve { hostname, port, password, dir }) => {
            let app = app::App::init(dir.unwrap_or_else(cwd)).await?;
            server::serve(app, server::Options { hostname, port, password }).await
        }
        Some(Cmd::Mini { project, tui }) => tui::start(project.unwrap_or_else(cwd), tui.into()).await,
        Some(Cmd::Run { message, continue_last, session, fork, model, agent, format, file, title, thinking, auto_approve, dir }) => {
            let app = app::App::init(dir.unwrap_or_else(cwd)).await?;
            run::run(app, run::RunArgs { message, continue_last, session, fork, model, agent, format, files: file, title, thinking, auto_approve }).await
        }
        Some(Cmd::Models { provider, verbose, refresh }) => {
            if refresh {
                models::refresh().await?;
            }
            cli::models(provider, verbose).await
        }
        Some(Cmd::Auth { command }) => match command {
            AuthCmd::List => cli::auth_list().await,
            AuthCmd::Login { provider, key } => cli::auth_login(provider, key).await,
            AuthCmd::Logout { provider } => cli::auth_logout(provider).await,
        },
        Some(Cmd::Session { command }) => match command {
            SessionCmd::List { max_count, format } => cli::session_list(max_count, &format).await,
            SessionCmd::Delete { session_id } => cli::session_delete(&session_id).await,
            SessionCmd::Export { session, sanitize } => cli::session_export(session, sanitize).await,
            SessionCmd::Import { file } => cli::session_import(&file).await,
        },
        Some(Cmd::Stats { days, json }) => cli::stats(days, json).await,
        Some(Cmd::Mcp { command: McpCmd::List }) => cli::mcp_list().await,
        Some(Cmd::Mcp { command: McpCmd::Add { name, url, header, env, global, command } }) => {
            cli::mcp_add(&name, url, &header, &env, global, &command)
        }
        Some(Cmd::Mcp { command: McpCmd::Auth { name } }) => cli::mcp_auth(&name).await,
        Some(Cmd::Mcp { command: McpCmd::Logout { name } }) => cli::mcp_logout(&name),
        Some(Cmd::Plugin { command }) => match command {
            PluginCmd::List => cli::plugin_list().await,
            PluginCmd::Add { package } => cli::plugin_add(&package),
            PluginCmd::Remove { package } => cli::plugin_remove(&package),
            PluginCmd::Check => cli::plugin_check().await,
            PluginCmd::Update => cli::plugin_update().await,
        },
        Some(Cmd::Debug { command }) => match command {
            DebugCmd::Paths => cli::debug_paths(),
            DebugCmd::Config => cli::debug_config().await,
            DebugCmd::Agents => cli::debug_agents().await,
            DebugCmd::Skills => cli::debug_skills().await,
            DebugCmd::Prompt => cli::debug_prompt().await,
            DebugCmd::Plugins => cli::debug_plugins().await,
            DebugCmd::PluginTool { name, input } => cli::debug_plugin_tool(&name, &input).await,
            DebugCmd::PluginTui { name, input } => cli::debug_plugin_tui(&name, &input).await,
            DebugCmd::Events { session } => cli::debug_events(session.as_deref()),
        },
    }
}

impl From<TuiArgs> for tui::StartOptions {
    fn from(a: TuiArgs) -> Self {
        tui::StartOptions { model: a.model, agent: a.agent, continue_last: a.continue_last, session: a.session, fork: a.fork, prompt: a.prompt }
    }
}
