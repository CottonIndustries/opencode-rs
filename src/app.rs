//! Shared application context: project, config, providers, agents, sessions,
//! the event bus consumed by the UIs, and interactive permission prompts.

use crate::agent::Agent;
use crate::config::{Config, Loaded};
use crate::models::Provider;
use crate::permission::{self, Effect, Reply};
use crate::session::{self, Message, Session};
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Updated { session: String },
    TextDelta { session: String, message_id: String, delta: String },
    ReasoningDelta { session: String, message_id: String, delta: String },
    ToolStarted { session: String, message_id: String, id: String, name: String, input: serde_json::Value },
    ToolFinished { session: String, message_id: String, id: String, name: String, title: String, output: String, error: Option<String>, metadata: serde_json::Value },
    StepFinished { session: String },
    Busy { session: String, busy: bool },
    Error { session: String, message: String },
    Retry {
        session: String,
        message: String,
        assistant_message_id: String,
        attempt: u32,
        at: i64,
    },
    Permission(permission::Request),
    PermissionResolved { id: String, session_id: String, reply: String },
    Title { session: String, title: String },
    Notice { message: String },
}

impl AppEvent {
    pub fn session(&self) -> Option<&str> {
        match self {
            AppEvent::Updated { session }
            | AppEvent::TextDelta { session, .. }
            | AppEvent::ReasoningDelta { session, .. }
            | AppEvent::ToolStarted { session, .. }
            | AppEvent::ToolFinished { session, .. }
            | AppEvent::StepFinished { session }
            | AppEvent::Busy { session, .. }
            | AppEvent::Error { session, .. }
            | AppEvent::Retry { session, .. }
            | AppEvent::Title { session, .. } => Some(session),
            AppEvent::Permission(r) => Some(&r.session_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskMode {
    /// Route "ask" decisions to an attached UI.
    Interactive,
    AutoApprove,
    AutoReject,
}

#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub delivery: Option<String>,
}

pub struct Handle {
    pub session: Mutex<Session>,
    pub cancel: Mutex<Option<CancellationToken>>,
    pub busy: AtomicBool,
    pub queue: Mutex<VecDeque<(Message, PromptOptions)>>,
    pub pending_move: Mutex<Option<std::path::PathBuf>>,
    pub pending_compaction: Mutex<Option<String>>,
    pub retry: Mutex<Option<serde_json::Value>>,
}

impl Handle {
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    pub fn id(&self) -> String {
        self.session.lock().unwrap().id.clone()
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut s = self.session.lock().unwrap();
        f(&mut s)
    }
    pub fn save(&self) {
        let mut s = self.session.lock().unwrap();
        s.time_updated = crate::util::now_ms();
        let (tokens, cost) = s.usage_total();
        s.tokens_total = tokens;
        s.cost_total = cost;
        if let Err(e) = session::save(&s) {
            eprintln!("failed to save session: {e}");
        }
    }
    pub fn abort(&self) -> Vec<String> {
        if let Some(c) = self.cancel.lock().unwrap().as_ref() {
            c.cancel();
        }
        let mut queue = self.queue.lock().unwrap();
        let cancelled = queue.iter().map(|(message, _)| message.id.clone()).collect();
        queue.clear();
        cancelled
    }
}

#[derive(Clone)]
struct PluginBase {
    providers: BTreeMap<String, Provider>,
    agents: BTreeMap<String, Agent>,
    commands: BTreeMap<String, crate::command::Command>,
}

pub struct App {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub is_git: bool,
    pub project: String,
    pub config: Config,
    pub config_files: Vec<PathBuf>,
    pub providers: RwLock<BTreeMap<String, Provider>>,
    pub agents: RwLock<BTreeMap<String, Agent>>,
    pub skills: RwLock<Vec<crate::tools::skill::Skill>>,
    pub commands: RwLock<BTreeMap<String, crate::command::Command>>,
    pub mcp: crate::mcp::Manager,
    pub schedules: crate::schedule::Scheduler,
    pub plugins: crate::plugin::Manager,
    pub plugin_commands: RwLock<std::collections::BTreeSet<String>>,
    plugin_default_agent: RwLock<Option<String>>,
    plugin_default_model: RwLock<Option<String>>,
    plugin_base: Mutex<Option<PluginBase>>,
    pub events: broadcast::Sender<AppEvent>,
    pub forms: crate::forms::FormStore,
    pub ask_mode: Mutex<AskMode>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    pending_requests: Mutex<HashMap<String, permission::Request>>,
    tool_progress: Mutex<HashMap<String, serde_json::Value>>,
    sessions: Mutex<HashMap<String, Arc<Handle>>>,
}

impl App {
    pub async fn init(directory: PathBuf) -> Result<Arc<App>> {
        Self::init_with_mcp(directory, BTreeMap::new()).await
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        project: String,
        directory: PathBuf,
        worktree: PathBuf,
        is_git: bool,
        config: crate::config::Config,
        config_files: Vec<PathBuf>,
        providers: BTreeMap<String, Provider>,
        agents: BTreeMap<String, Agent>,
        skills: Vec<crate::tools::skill::Skill>,
        commands: BTreeMap<String, crate::command::Command>,
        plugins: crate::plugin::Manager,
        plugin_commands: std::collections::BTreeSet<String>,
    ) -> Arc<App> {
        let mcp = crate::mcp::Manager::new(config.mcp.clone(), directory.clone());
        let (events, _) = broadcast::channel(4096);
        Arc::new(App {
            project,
            directory,
            worktree,
            is_git,
            config,
            config_files,
            providers: RwLock::new(providers),
            agents: RwLock::new(agents),
            skills: RwLock::new(skills),
            commands: RwLock::new(commands),
            mcp,
            schedules: crate::schedule::Scheduler::new(),
            plugins,
            plugin_commands: RwLock::new(plugin_commands),
            plugin_default_agent: RwLock::new(None),
            plugin_default_model: RwLock::new(None),
            plugin_base: Mutex::new(None),
            events,
            forms: crate::forms::FormStore::default(),
            ask_mode: Mutex::new(AskMode::Interactive),
            pending: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashMap::new()),
            tool_progress: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Like [`App::init`], adding MCP servers supplied by a client (ACP) to the configured ones.
    pub async fn init_with_mcp(directory: PathBuf, extra_mcp: BTreeMap<String, crate::config::McpConfig>) -> Result<Arc<App>> {
        crate::paths::get().ensure();
        let directory = std::fs::canonicalize(&directory).map(|p| strip_unc(&p)).unwrap_or(directory);
        let found = crate::config::find_worktree(&directory);
        let is_git = found.is_some();
        let worktree = found.unwrap_or_else(|| directory.clone());
        let Loaded { mut config, files, directories, plugins: specs } = crate::config::load(&directory, &worktree)?;
        config.mcp.extend(extra_mcp);
        let project = session::project_id(&worktree);
        let mut providers = crate::models::providers(&config);
        let mut agents = crate::agent::load(&config, &directories);
        let mut skills = crate::tools::skill::discover(&config, &directories, &directory);
        let mut commands = crate::command::load(&config, &directories);
        let base = PluginBase {
            providers: providers.clone(),
            agents: agents.clone(),
            commands: commands.clone(),
        };

        // Plugins can transform the registries before the application starts.
        // The host receives a JSON snapshot and returns the transformed state.
        // A provisional app lets plugin setup read live registries through the
        // client APIs while the final app is still being assembled.
        let snapshot = crate::plugin::snapshot(&config, &providers, &agents, &skills, &commands, &directory, &worktree, &project);
        let provisional = Self::assemble(
            project.clone(),
            directory.clone(),
            worktree.clone(),
            is_git,
            config.clone(),
            files.clone(),
            providers.clone(),
            agents.clone(),
            skills.clone(),
            commands.clone(),
            crate::plugin::Manager::disabled(),
            Default::default(),
        );
        let (plugin_manager, state, errors) =
            crate::plugin::init(&directory, &worktree, &project, specs, snapshot, Some(&provisional)).await;
        drop(provisional);
        for error in errors {
            eprintln!("warning: {error}");
        }
        if plugin_manager.enabled() {
            // A legacy `config` hook may have edited the snapshot. The snapshot
            // originated from this config, so decoding it back is lossless for
            // every field this build understands.
            if let Some(value) = state.get("config") {
                if let Ok(updated) = serde_json::from_value::<crate::config::Config>(value.clone()) {
                    config = updated;
                }
            }
            let applied = crate::plugin::apply_state(&state, &config, &providers, &agents, &commands);
            providers = applied.providers;
            agents = applied.agents;
            skills = applied.skills;
            commands = applied.commands;
            config.mcp = applied.mcp;
            if let Some(agent) = applied.default_agent {
                if agents.contains_key(&agent) {
                    config.default_agent = Some(agent);
                }
            }
            if let Some(model) = applied.default_model {
                config.model = Some(model);
            }
            let app = Self::assemble(
                project,
                directory,
                worktree,
                is_git,
                config,
                files,
                providers,
                agents,
                skills,
                commands,
                plugin_manager,
                applied.plugin_commands,
            );
            *app.plugin_base.lock().unwrap() = Some(base);
            app.plugins.attach(&app);
            app.load_mcp_prompts().await;
            if app.config.experimental.schedules {
                crate::schedule::start(app.clone());
            }
            return Ok(app);
        }

        let app = Self::assemble(
            project,
            directory,
            worktree,
            is_git,
            config,
            files,
            providers,
            agents,
            skills,
            commands,
            plugin_manager,
            Default::default(),
        );
        *app.plugin_base.lock().unwrap() = Some(base);
        app.plugins.attach(&app);
        app.load_mcp_prompts().await;
        if app.config.experimental.schedules {
            crate::schedule::start(app.clone());
        }
        Ok(app)
    }

    /// Register MCP prompts as slash commands. Best effort: a server that is
    /// offline at startup simply contributes no commands.
    pub async fn load_mcp_prompts(&self) {
        if self.mcp.configured() == 0 {
            return;
        }
        let Ok(prompts) = self.mcp.prompts().await else { return };
        let mut commands = self.commands.write().unwrap();
        for prompt in prompts {
            let (Some(server), Some(name)) = (
                prompt.get("server").and_then(serde_json::Value::as_str),
                prompt.get("name").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let arguments = prompt
                .get("arguments")
                .and_then(serde_json::Value::as_array)
                .map(|arguments| {
                    arguments
                        .iter()
                        .filter_map(|argument| argument.get("name").and_then(serde_json::Value::as_str).map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let command_name = crate::command::mcp_command_name(server, name);
            commands.insert(
                command_name.clone(),
                crate::command::Command {
                    name: command_name,
                    description: prompt.get("description").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
                    template: String::new(),
                    agent: None,
                    model: None,
                    mcp_prompt: Some((server.to_string(), name.to_string(), arguments)),
                },
            );
        }
    }

    /// Apply a transformed plugin state after startup (transform `reload()`).
    pub fn apply_plugin_state(&self, state: &serde_json::Value) {
        let base = self.plugin_base.lock().unwrap().clone();
        let (base_providers, base_agents, base_commands) = match base {
            Some(base) => (base.providers, base.agents, base.commands),
            None => (self.providers(), self.agents(), self.commands()),
        };
        let applied = crate::plugin::apply_state(state, &self.config, &base_providers, &base_agents, &base_commands);
        *self.providers.write().unwrap() = applied.providers;
        *self.agents.write().unwrap() = applied.agents;
        *self.skills.write().unwrap() = applied.skills;
        *self.commands.write().unwrap() = applied.commands;
        *self.plugin_commands.write().unwrap() = applied.plugin_commands;
        *self.plugin_default_agent.write().unwrap() = applied.default_agent;
        *self.plugin_default_model.write().unwrap() = applied.default_model;
        self.mcp.reconfigure(applied.mcp);
    }

    /// Start a turn in the background (plugin/client requests are admitted,
    /// not waited for).
    pub fn spawn_prompt(self: &Arc<Self>, session_id: &str, msg: Message, opts: PromptOptions) -> Result<()> {
        let app = self.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            let _ = crate::engine::prompt(app, &session_id, msg, opts).await;
        });
        Ok(())
    }

    pub fn emit(&self, ev: AppEvent) {
        match &ev {
            AppEvent::Retry { session, message, assistant_message_id, attempt, at } => {
                if let Some(handle) = self.sessions.lock().unwrap().get(session) {
                    *handle.retry.lock().unwrap() = Some(serde_json::json!({
                        "type": "retry",
                        "assistantMessageID": assistant_message_id,
                        "attempt": attempt,
                        "message": message,
                        "at": at,
                    }));
                }
            }
            AppEvent::Busy { session, .. } => {
                if let Some(handle) = self.sessions.lock().unwrap().get(session) {
                    *handle.retry.lock().unwrap() = None;
                }
            }
            _ => {}
        }
        let mut plugin_event = plugin_event(&ev);
        if let Some(session_id) = plugin_event.pointer("/properties/sessionID").and_then(serde_json::Value::as_str) {
            let kind = plugin_event.get("type").and_then(serde_json::Value::as_str).unwrap_or_default();
            if matches!(kind, "session.updated" | "session.idle" | "session.status" | "session.created") {
                let session = self.sessions.lock().unwrap().get(session_id).map(|h| h.with(|s| s.clone()));
                if let Some(session) = session {
                    if let Some(properties) = plugin_event.get_mut("properties").and_then(serde_json::Value::as_object_mut) {
                        properties.insert("info".into(), crate::plugin::session_json(&session));
                    }
                }
            }
        }
        self.plugins.emit(plugin_event);
        let _ = self.events.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    pub fn providers(&self) -> BTreeMap<String, Provider> {
        self.providers.read().unwrap().clone()
    }

    pub fn session_statuses(&self) -> Vec<(String, bool, Option<serde_json::Value>)> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|handle| (handle.id(), handle.is_busy(), handle.retry.lock().unwrap().clone()))
            .collect()
    }

    pub fn active_session_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|handle| handle.is_busy())
            .map(|handle| handle.id())
            .collect()
    }

    pub fn agent(&self, id: &str) -> Result<Agent> {
        self.agents
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("agent not found: {id}"))
    }

    pub fn agents(&self) -> BTreeMap<String, Agent> {
        self.agents.read().unwrap().clone()
    }

    pub fn skills(&self) -> Vec<crate::tools::skill::Skill> {
        self.skills.read().unwrap().clone()
    }

    pub fn commands(&self) -> BTreeMap<String, crate::command::Command> {
        self.commands.read().unwrap().clone()
    }

    pub fn plugin_commands(&self) -> std::collections::BTreeSet<String> {
        self.plugin_commands.read().unwrap().clone()
    }

    pub fn default_agent(&self) -> String {
        if let Some(id) = self.plugin_default_agent.read().unwrap().as_ref() {
            if self.agents.read().unwrap().get(id).is_some_and(|a| a.is_primary() && !a.hidden) {
                return id.clone();
            }
        }
        crate::agent::default_agent(&self.config, &self.agents.read().unwrap())
    }

    pub fn primary_agents(&self) -> Vec<Agent> {
        let mut v: Vec<Agent> = self
            .agents
            .read()
            .unwrap()
            .values()
            .filter(|a| a.is_primary() && !a.hidden)
            .cloned()
            .collect();
        v.sort_by_key(|a| (a.id != "build", a.id.clone()));
        v
    }

    pub fn default_model(&self) -> Option<String> {
        if let Some(reference) = self.plugin_default_model.read().unwrap().as_ref() {
            if crate::models::resolve(&self.providers.read().unwrap(), reference).is_ok() {
                return Some(reference.clone());
            }
        }
        crate::models::default_model(&self.config, &self.providers.read().unwrap())
    }

    fn insert(&self, s: Session) -> Arc<Handle> {
        let id = s.id.clone();
        let h = Arc::new(Handle {
            session: Mutex::new(s),
            cancel: Mutex::new(None),
            busy: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::new()),
            pending_move: Mutex::new(None),
            pending_compaction: Mutex::new(None),
            retry: Mutex::new(None),
        });
        self.sessions.lock().unwrap().insert(id, h.clone());
        h
    }

    pub fn create_session(&self, parent: Option<String>) -> Arc<Handle> {
        let h = self.new_session(parent);
        h.save();
        let session = h.with(|s| s.clone());
        self.plugins.emit(serde_json::json!({
            "id": crate::util::id("evt"),
            "type": "session.created",
            "properties": { "info": crate::plugin::session_json(&session) }
        }));
        h
    }

    /// A session kept in memory until its first message is saved.
    pub fn new_session(&self, parent: Option<String>) -> Arc<Handle> {
        self.insert(Session::new(&self.project, &self.directory, parent))
    }

    pub fn session(&self, id: &str) -> Result<Arc<Handle>> {
        if let Some(h) = self.sessions.lock().unwrap().get(id) {
            return Ok(h.clone());
        }
        let s = session::load(&self.project, id)?;
        Ok(self.insert(s))
    }

    pub fn forget_session(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    /// Permission check: evaluates rules for the agent and session, prompting when needed.
    pub async fn authorize(
        &self,
        handle: &Handle,
        agent: &Agent,
        tool_call_id: Option<&str>,
        action: &str,
        resources: &[String],
        save: &[String],
        title: String,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let mut rules = permission::defaults();
        if let Some(p) = &self.config.permission {
            rules.extend(permission::from_config(p));
        }
        rules.extend(agent.permissions.iter().cloned());
        if let Some(session_permissions) = handle.with(|s| s.permissions.clone()) {
            rules.extend(permission::from_config(&session_permissions));
        }
        let approved: Vec<(String, String)> = handle.with(|s| s.approved.clone());
        rules.extend(approved.iter().map(|(a, r)| permission::rule(a, r, Effect::Allow)));

        let mut asks = false;
        for resource in resources {
            match permission::evaluate(&rules, action, resource) {
                Effect::Deny => {
                    return Err(anyhow!(
                        "Permission denied: the {action} permission for {resource} is denied by the configured rules. Do not retry; continue without it or ask the user."
                    ))
                }
                Effect::Ask => asks = true,
                Effect::Allow => {}
            }
        }
        let configured = if asks { Effect::Ask } else { Effect::Allow };
        let event = self
            .plugins
            .trigger(
                "permission",
                "evaluate",
                serde_json::json!({
                    "sessionID": handle.id(),
                    "agent": agent.id,
                    "action": action,
                    "resources": resources,
                    "metadata": metadata.clone(),
                    "effect": match configured {
                        Effect::Allow => "allow",
                        Effect::Deny => "deny",
                        Effect::Ask => "ask",
                    },
                    "request": {
                        "id": crate::util::id("per"),
                        "sessionID": handle.id(),
                        "permission": action,
                        "patterns": resources,
                        "always": save,
                        "metadata": metadata.clone(),
                    },
                }),
            )
            .await;
        let denial_message = event.get("message").and_then(serde_json::Value::as_str).map(String::from);
        let effect = event
            .get("effect")
            .and_then(serde_json::Value::as_str)
            .and_then(Effect::parse)
            .unwrap_or(configured);
        match effect {
            Effect::Allow => return Ok(()),
            Effect::Deny => {
                if let Some(message) = denial_message {
                    return Err(anyhow!("Permission denied: {message}"));
                }
                return Err(anyhow!(
                    "Permission denied: the {action} permission for {} is denied by a plugin. Do not retry; continue without it or ask the user.",
                    resources.join(", ")
                ));
            }
            Effect::Ask => {}
        }
        let needs_ask = resources.to_vec();
        let mode = *self.ask_mode.lock().unwrap();
        match mode {
            AskMode::AutoApprove => return Ok(()),
            AskMode::AutoReject => {
                return Err(anyhow!(
                    "Permission rejected: {action} on {} requires approval, which is unavailable in this mode.",
                    needs_ask.join(", ")
                ))
            }
            AskMode::Interactive => {}
        }
        let session_id = handle.id();
        let req = permission::Request {
            id: crate::util::id("per"),
            session_id: session_id.clone(),
            tool_call_id: tool_call_id.map(String::from),
            action: action.to_string(),
            resources: needs_ask.clone(),
            title,
            metadata,
            save: if save.is_empty() { needs_ask.clone() } else { save.to_vec() },
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req.id.clone(), tx);
        self.pending_requests.lock().unwrap().insert(req.id.clone(), req.clone());
        let save = req.save.clone();
        let id = req.id.clone();
        self.emit(AppEvent::Permission(req));
        let reply = rx.await.unwrap_or(Reply::Reject);
        self.pending_requests.lock().unwrap().remove(&id);
        self.emit(AppEvent::PermissionResolved {
            id,
            session_id: session_id.clone(),
            reply: match reply {
                Reply::Once => "once",
                Reply::Always => "always",
                Reply::Reject => "reject",
            }.into(),
        });
        match reply {
            Reply::Once => Ok(()),
            Reply::Always => {
                handle.with(|s| {
                    for p in &save {
                        s.approved.push((action.to_string(), p.clone()));
                    }
                });
                handle.save();
                Ok(())
            }
            Reply::Reject => Err(anyhow!(crate::engine::REJECTED)),
        }
    }

    pub fn reply_permission(&self, id: &str, reply: Reply) {
        self.pending_requests.lock().unwrap().remove(id);
        if let Some(tx) = self.pending.lock().unwrap().remove(id) {
            let _ = tx.send(reply);
        }
    }

    pub fn pending_permissions(&self) -> Vec<permission::Request> {
        self.pending_requests.lock().unwrap().values().cloned().collect()
    }

    pub fn record_tool_progress(&self, call_id: &str, metadata: serde_json::Value) {
        if call_id.is_empty() || metadata.is_null() {
            return;
        }
        let mut progress = self.tool_progress.lock().unwrap();
        match (progress.get_mut(call_id), metadata) {
            (Some(existing), serde_json::Value::Object(next)) => {
                if let Some(target) = existing.as_object_mut() {
                    for (key, value) in next {
                        target.insert(key, value);
                    }
                } else {
                    *existing = serde_json::Value::Object(next);
                }
            }
            (_, next) => {
                progress.insert(call_id.to_string(), next);
            }
        }
    }

    pub fn take_tool_progress(&self, call_id: &str) -> Option<serde_json::Value> {
        self.tool_progress.lock().unwrap().remove(call_id)
    }

    pub fn reject_all_pending(&self, session_id: &str) {
        let _ = session_id;
        self.pending_requests.lock().unwrap().clear();
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Reply::Reject);
        }
    }
}

pub(crate) fn plugin_event(ev: &AppEvent) -> serde_json::Value {
    use serde_json::json;
    let id = || crate::util::id("evt");
    match ev {
        AppEvent::Updated { session } => json!({
            "id": id(), "type": "session.updated", "properties": { "sessionID": session }
        }),
        AppEvent::TextDelta { session, message_id, delta } => json!({
            "id": id(), "type": "message.part.updated", "properties": {
                "sessionID": session, "assistantMessageID": message_id, "delta": delta,
                "part": { "type": "text", "text": delta }
            }
        }),
        AppEvent::ReasoningDelta { session, message_id, delta } => json!({
            "id": id(), "type": "message.part.updated", "properties": {
                "sessionID": session, "assistantMessageID": message_id, "delta": delta,
                "part": { "type": "reasoning", "text": delta }
            }
        }),
        AppEvent::ToolStarted { session, message_id, id: call, name, input } => json!({
            "id": id(), "type": "message.part.updated", "properties": {
                "sessionID": session, "assistantMessageID": message_id, "callID": call,
                "part": { "type": "tool", "tool": name, "callID": call, "state": { "status": "running", "input": input } }
            }
        }),
        AppEvent::ToolFinished { session, message_id, id: call, name, title, output, error, metadata } => json!({
            "id": id(), "type": "message.part.updated", "properties": {
                "sessionID": session, "assistantMessageID": message_id, "callID": call,
                "part": { "type": "tool", "tool": name, "callID": call, "state": if let Some(error) = error {
                    json!({ "status": "error", "error": error })
                } else {
                    json!({ "status": "completed", "title": title, "output": output, "metadata": metadata })
                }}
            }
        }),
        AppEvent::Busy { session, busy } => {
            if *busy {
                json!({ "id": id(), "type": "session.status", "properties": { "sessionID": session, "status": { "type": "busy" } } })
            } else {
                json!({ "id": id(), "type": "session.idle", "properties": { "sessionID": session } })
            }
        }
        AppEvent::Error { session, message } => json!({
            "id": id(), "type": "session.error", "properties": { "sessionID": session, "error": { "message": message } }
        }),
        AppEvent::Retry { session, message, assistant_message_id, attempt, at } => json!({
            "id": id(),
            "type": "session.retry",
            "properties": {
                "sessionID": session,
                "assistantMessageID": assistant_message_id,
                "attempt": attempt,
                "at": at,
                "error": { "type": "unknown", "message": message },
            }
        }),
        AppEvent::Permission(request) => json!({
            "id": id(), "type": "permission.asked", "properties": {
                "id": request.id,
                "sessionID": request.session_id,
                "action": request.action,
                "resources": request.resources,
                "save": request.save,
                "metadata": request.metadata,
                "message": request.title,
            }
        }),
        AppEvent::PermissionResolved { id: request, session_id, reply } => json!({
            "id": id(), "type": "permission.replied", "properties": {
                "sessionID": session_id, "requestID": request, "reply": reply
            }
        }),
        AppEvent::Title { session, title } => json!({
            "id": id(), "type": "session.renamed", "properties": { "sessionID": session, "title": title }
        }),
        AppEvent::StepFinished { session } => json!({
            "id": id(), "type": "session.step", "properties": { "sessionID": session }
        }),
        AppEvent::Notice { message } => json!({
            "id": id(), "type": "server.notice", "properties": { "message": message }
        }),
    }
}

/// `\\?\C:\x` → `C:\x` on Windows.
pub fn strip_unc(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC") {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}
