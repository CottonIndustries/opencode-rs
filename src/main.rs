//! OpenCode — AI coding agent for the terminal (Rust rewrite).

mod acp;
mod agent;
mod app;
mod auth;
mod cli;
mod command;
mod config;
mod engine;
mod http;
mod mcp;
mod models;
mod paths;
mod permission;
mod plugin;
mod prompt;
mod provider;
mod run;
mod session;
mod state;
mod tools;
mod tui;
mod util;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Upstream release this port mirrors; reported to providers like the TypeScript client does.
pub const UPSTREAM_VERSION: &str = "2.1.4";
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
        Some(Cmd::Debug { command }) => match command {
            DebugCmd::Paths => cli::debug_paths(),
            DebugCmd::Config => cli::debug_config().await,
            DebugCmd::Agents => cli::debug_agents().await,
            DebugCmd::Skills => cli::debug_skills().await,
            DebugCmd::Prompt => cli::debug_prompt().await,
            DebugCmd::Plugins => cli::debug_plugins().await,
            DebugCmd::PluginTool { name, input } => cli::debug_plugin_tool(&name, &input).await,
        },
    }
}

impl From<TuiArgs> for tui::StartOptions {
    fn from(a: TuiArgs) -> Self {
        tui::StartOptions { model: a.model, agent: a.agent, continue_last: a.continue_last, session: a.session, fork: a.fork, prompt: a.prompt }
    }
}
