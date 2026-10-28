//! Interactive terminal UI.

pub mod render;

use crate::app::{App, AppEvent, Handle, PromptOptions};
use crate::permission::{self, Reply};
use crate::session::{Message, Role};
use anyhow::Result;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use render::THEME;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

pub struct StartOptions {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub continue_last: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub prompt: Option<String>,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Default)]
struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }
    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0)
    }
    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8()).unwrap_or(self.cursor)
    }
    fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev_boundary();
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }
    fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next_boundary();
            self.text.replace_range(self.cursor..n, "");
        }
    }
    fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }
    fn right(&mut self) {
        self.cursor = self.next_boundary();
    }
    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }
    fn home(&mut self) {
        self.cursor = self.line_start();
    }
    fn end(&mut self) {
        self.cursor = self.line_end();
    }
    /// Move up a line; false when already on the first line.
    fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let prev_line = &self.text[prev_start..start - 1];
        self.cursor = prev_start + prev_line.chars().take(col).map(|c| c.len_utf8()).sum::<usize>();
        true
    }
    fn down(&mut self) -> bool {
        let end = self.line_end();
        if end >= self.text.len() {
            return false;
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..].find('\n').map(|i| next_start + i).unwrap_or(self.text.len());
        self.cursor = next_start + self.text[next_start..next_end].chars().take(col).map(|c| c.len_utf8()).sum::<usize>();
        true
    }
    fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
        let start = trimmed.rfind(|c: char| c.is_whitespace() || "/\\.,;:()[]{}".contains(c)).map(|i| i + 1).unwrap_or(0);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
    fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }
    /// Wrapped display lines and the cursor position (row, col).
    fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut cursor = (0, 0);
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut row = String::new();
            let mut w = 0;
            let mut pos = offset;
            for c in line.chars() {
                let cw = UnicodeWidthStr::width(c.encode_utf8(&mut [0; 4]) as &str);
                if w + cw > width {
                    rows.push(std::mem::take(&mut row));
                    w = 0;
                }
                if pos == self.cursor {
                    cursor = (rows.len(), w);
                }
                row.push(c);
                w += cw;
                pos += c.len_utf8();
            }
            if pos == self.cursor {
                if w >= width {
                    rows.push(std::mem::take(&mut row));
                    w = 0;
                }
                cursor = (rows.len(), w);
            }
            rows.push(row);
            offset += line.len() + 1;
        }
        (rows, cursor)
    }
}

#[derive(Clone)]
struct Item {
    label: String,
    value: String,
    detail: String,
}

#[derive(Clone, Copy, PartialEq)]
enum PickerKind {
    Models,
    Sessions,
    Agents,
    Commands,
}

struct Picker {
    kind: PickerKind,
    title: String,
    items: Vec<Item>,
    filter: String,
    selected: usize,
}

impl Picker {
    fn visible(&self) -> Vec<&Item> {
        let f = self.filter.to_lowercase();
        let terms: Vec<&str> = f.split_whitespace().collect();
        self.items
            .iter()
            .filter(|i| {
                let hay = format!("{} {} {}", i.label, i.value, i.detail).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .collect()
    }
}

enum Dialog {
    Picker(Picker),
    Help,
}

struct Ui {
    app: Arc<App>,
    handle: Arc<Handle>,
    input: Input,
    scroll: usize,
    agent: String,
    model: Option<String>,
    variant: Option<String>,
    dialog: Option<Dialog>,
    permissions: VecDeque<permission::Request>,
    notice: Option<(String, Instant, bool)>,
    retry: Option<String>,
    thinking: bool,
    details: bool,
    frame: usize,
    quit: bool,
    history: Vec<String>,
    history_idx: Option<usize>,
    cache: HashMap<String, (u64, usize, Vec<Line<'static>>)>,
    body_height: usize,
    last_ctrl_c: Option<Instant>,
}

/// Cheap structural fingerprint: changes whenever streamed content or tool state changes.
fn message_key(m: &Message) -> u64 {
    use crate::session::{Part, ToolState};
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    m.parts.len().hash(&mut h);
    m.error.hash(&mut h);
    for p in &m.parts {
        match p {
            Part::Text { text, synthetic } => (0u8, text.len(), *synthetic).hash(&mut h),
            Part::Reasoning { text, .. } => (1u8, text.len()).hash(&mut h),
            Part::Tool { input, state, .. } => {
                let s = match state {
                    ToolState::Pending => (0u8, 0usize),
                    ToolState::Running => (1, 0),
                    ToolState::Completed { output, .. } => (2, output.len()),
                    ToolState::Error { error } => (3, error.len()),
                };
                (2u8, input.as_object().map_or(0, |o| o.len()), s).hash(&mut h)
            }
            Part::File(f) => (3u8, f.data.len()).hash(&mut h),
            Part::Compaction { summary } => (4u8, summary.len()).hash(&mut h),
        }
    }
    h.finish()
}

impl Ui {
    fn new(app: Arc<App>, opts: &StartOptions) -> Result<Ui> {
        let handle = if let Some(id) = &opts.session {
            app.session(id).unwrap_or_else(|_| app.new_session(None))
        } else if opts.continue_last {
            match crate::session::list(&app.project).into_iter().find(|s| s.parent_id.is_none()) {
                Some(s) => app.session(&s.id)?,
                None => app.new_session(None),
            }
        } else {
            app.new_session(None)
        };
        let handle = if opts.fork && (opts.session.is_some() || opts.continue_last) {
            let f = crate::session::fork(&handle.session.lock().unwrap());
            crate::session::save(&f)?;
            app.session(&f.id)?
        } else {
            handle
        };
        let agent = opts.agent.clone().or_else(|| handle.with(|s| s.agent.clone())).unwrap_or_else(|| app.default_agent());
        let model_ref = opts.model.clone().or_else(|| handle.with(|s| s.model.clone())).or_else(|| app.agents.get(&agent).and_then(|a| a.model.clone())).or_else(|| app.default_model());
        let (model, variant) = match model_ref {
            Some(r) => match r.split_once('#') {
                Some((m, v)) => (Some(m.to_string()), Some(v.to_string())),
                None => (Some(r), None),
            },
            None => (None, None),
        };
        let st = crate::state::get();
        let mut ui = Ui {
            app,
            handle,
            input: Input::default(),
            scroll: 0,
            agent,
            model,
            variant,
            dialog: None,
            permissions: VecDeque::new(),
            notice: None,
            retry: None,
            thinking: st.show_thinking,
            details: st.show_details.unwrap_or(false),
            frame: 0,
            quit: false,
            history: Vec::new(),
            history_idx: None,
            cache: HashMap::new(),
            body_height: 20,
            last_ctrl_c: None,
        };
        if ui.model.is_none() {
            ui.notify("No model configured. Set a provider API key (e.g. ANTHROPIC_API_KEY) or run `opencode auth login`, then pick one with /models.", true);
        }
        Ok(ui)
    }

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.notice = Some((msg.into(), Instant::now(), error));
    }

    fn model_ref(&self) -> Option<String> {
        self.model.as_ref().map(|m| match &self.variant {
            Some(v) => format!("{m}#{v}"),
            None => m.clone(),
        })
    }

    fn current_model(&self) -> Option<crate::models::Model> {
        let m = self.model.as_ref()?;
        crate::models::resolve(&self.app.providers.read().unwrap(), m).ok().map(|r| r.model)
    }

    fn sid(&self) -> String {
        self.handle.id()
    }

    // ---- actions ----

    fn submit(&mut self) {
        let text = self.input.text.trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.history_idx = None;
        self.input.clear();
        self.scroll = 0;
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if self.slash(name, args.trim()) {
                return;
            }
        }
        self.send(text, None);
    }

    fn send(&mut self, text: String, cmd: Option<&crate::command::Command>) {
        if self.model.is_none() && cmd.and_then(|c| c.model.clone()).is_none() {
            self.notify("No model selected. Use /models to choose one.", true);
            return;
        }
        let mut msg = Message::user_text(text.clone());
        msg.parts.extend(crate::run::expand_mentions(&self.app, &text));
        let opts = PromptOptions {
            agent: cmd.and_then(|c| c.agent.clone()).or_else(|| Some(self.agent.clone())),
            model: cmd.and_then(|c| c.model.clone()).or_else(|| self.model.clone()),
            variant: self.variant.clone(),
        };
        let app = self.app.clone();
        let sid = self.sid();
        tokio::spawn(async move {
            let _ = crate::engine::prompt(app, &sid, msg, opts).await;
        });
    }

    /// Handle a slash command; returns true when consumed.
    fn slash(&mut self, name: &str, args: &str) -> bool {
        match name {
            "new" | "clear" => self.new_session(),
            "sessions" | "resume" | "continue" => self.open_sessions(),
            "models" | "model" => self.open_models(),
            "agents" | "agent" => self.open_agents(),
            "compact" | "summarize" => self.compact(),
            "thinking" => self.toggle_thinking(),
            "details" => self.toggle_details(),
            "export" => self.export(),
            "help" => self.dialog = Some(Dialog::Help),
            "variant" => self.cycle_variant(),
            "exit" | "quit" | "q" => self.quit = true,
            _ => {
                if let Some(cmd) = self.app.commands.get(name).cloned() {
                    let text = crate::command::render(&cmd, args, &self.app.directory, &self.app.worktree);
                    self.send(text, Some(&cmd));
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn new_session(&mut self) {
        if self.handle.is_busy() {
            self.handle.abort();
        }
        self.handle = self.app.new_session(None);
        self.cache.clear();
        self.scroll = 0;
        self.notify("New session", false);
    }

    fn compact(&mut self) {
        let app = self.app.clone();
        let sid = self.sid();
        tokio::spawn(async move {
            if let Err(e) = crate::engine::compact_session(app.clone(), &sid).await {
                app.emit(AppEvent::Error { session: sid, message: e.to_string() });
            }
        });
    }

    fn toggle_thinking(&mut self) {
        self.thinking = !self.thinking;
        let v = self.thinking;
        crate::state::update(|s| s.show_thinking = v);
        self.cache.clear();
        self.notify(if v { "Thinking blocks shown" } else { "Thinking blocks hidden" }, false);
    }

    fn toggle_details(&mut self) {
        self.details = !self.details;
        let v = self.details;
        crate::state::update(|s| s.show_details = Some(v));
        self.cache.clear();
        self.notify(if v { "Tool details shown" } else { "Tool details hidden" }, false);
    }

    fn cycle_variant(&mut self) {
        let Some(model) = self.current_model() else { return };
        let vs = model.variants();
        if vs.is_empty() {
            self.notify(format!("{} has no variants", model.full_id()), false);
            return;
        }
        self.variant = match &self.variant {
            None => Some(vs[0].clone()),
            Some(v) => vs.iter().position(|x| x == v).and_then(|i| vs.get(i + 1)).cloned(),
        };
        self.notify(format!("Variant: {}", self.variant.as_deref().unwrap_or("default")), false);
    }

    fn cycle_agent(&mut self, back: bool) {
        let agents: Vec<String> = self.app.primary_agents().iter().map(|a| a.id.clone()).collect();
        if agents.is_empty() {
            return;
        }
        let i = agents.iter().position(|a| *a == self.agent).unwrap_or(0);
        let n = agents.len();
        self.agent = agents[if back { (i + n - 1) % n } else { (i + 1) % n }].clone();
        if let Some(m) = self.app.agents.get(&self.agent).and_then(|a| a.model.clone()) {
            self.model = Some(m);
        }
    }

    fn export(&mut self) {
        let s = self.handle.session.lock().unwrap().clone();
        let mut md = format!("# {}\n\n", s.title);
        for m in &s.messages {
            match m.role {
                Role::User => md.push_str(&format!("## User\n\n{}\n\n", m.text())),
                Role::Assistant => {
                    md.push_str("## Assistant\n\n");
                    for p in &m.parts {
                        match p {
                            crate::session::Part::Text { text, .. } => md.push_str(&format!("{text}\n\n")),
                            crate::session::Part::Tool { name, input, .. } => md.push_str(&format!("**Tool: {name}** `{}`\n\n", render::tool_summary(name, input))),
                            _ => {}
                        }
                    }
                }
            }
        }
        let path = self.app.directory.join(format!("session-{}.md", s.id));
        match std::fs::write(&path, md) {
            Ok(_) => self.notify(format!("Exported to {}", path.display()), false),
            Err(e) => self.notify(format!("Export failed: {e}"), true),
        }
    }

    fn open_models(&mut self) {
        let providers = self.app.providers();
        let available = crate::models::available(&self.app.config, &providers);
        let recent = crate::state::get().recent_models;
        let mut items = Vec::new();
        for r in &recent {
            let base = r.split('#').next().unwrap_or(r);
            if let Ok(res) = crate::models::resolve(&providers, base) {
                items.push(Item { label: format!("{}  (recent)", res.model.name), value: base.to_string(), detail: base.to_string() });
            }
        }
        for id in &available {
            let p = &providers[id];
            let mut ms: Vec<_> = p.models.values().filter(|m| m.tool_call).collect();
            ms.sort_by(|a, b| b.release_date.cmp(&a.release_date));
            for m in ms {
                let full = m.full_id();
                if items.iter().any(|i| i.value == full) {
                    continue;
                }
                let free = if m.cost_input == 0.0 && m.cost_output == 0.0 { "  free" } else { "" };
                items.push(Item { label: m.name.clone(), value: full.clone(), detail: format!("{}{free}", p.name) });
            }
        }
        if items.is_empty() {
            self.notify("No providers with credentials. Set an API key or run `opencode auth login`.", true);
            return;
        }
        self.dialog = Some(Dialog::Picker(Picker { kind: PickerKind::Models, title: "Select model".into(), items, filter: String::new(), selected: 0 }));
    }

    fn open_sessions(&mut self) {
        let items: Vec<Item> = crate::session::list(&self.app.project)
            .into_iter()
            .filter(|s| s.parent_id.is_none())
            .map(|s| Item {
                label: s.title,
                value: s.id,
                detail: chrono::DateTime::from_timestamp_millis(s.time_updated).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
            })
            .collect();
        if items.is_empty() {
            self.notify("No sessions yet", false);
            return;
        }
        self.dialog = Some(Dialog::Picker(Picker { kind: PickerKind::Sessions, title: "Sessions  (ctrl+d delete)".into(), items, filter: String::new(), selected: 0 }));
    }

    fn open_agents(&mut self) {
        let items = self
            .app
            .primary_agents()
            .iter()
            .map(|a| Item { label: a.name.clone(), value: a.id.clone(), detail: a.description.clone().unwrap_or_default() })
            .collect();
        self.dialog = Some(Dialog::Picker(Picker { kind: PickerKind::Agents, title: "Select agent".into(), items, filter: String::new(), selected: 0 }));
    }

    fn open_commands(&mut self) {
        let mut items: Vec<Item> = [
            ("New session", "/new", "ctrl+n"),
            ("Switch session", "/sessions", "ctrl+l"),
            ("Switch model", "/models", "ctrl+o"),
            ("Switch agent", "/agents", "tab"),
            ("Cycle model variant", "/variant", "ctrl+t"),
            ("Compact conversation", "/compact", ""),
            ("Toggle thinking blocks", "/thinking", ""),
            ("Toggle tool details", "/details", ""),
            ("Export conversation to markdown", "/export", ""),
            ("Help", "/help", ""),
            ("Quit", "/quit", "ctrl+c"),
        ]
        .into_iter()
        .map(|(l, v, d)| Item { label: l.into(), value: v.into(), detail: d.into() })
        .collect();
        for c in self.app.commands.values() {
            items.push(Item { label: format!("/{}", c.name), value: format!("/{}", c.name), detail: c.description.clone() });
        }
        self.dialog = Some(Dialog::Picker(Picker { kind: PickerKind::Commands, title: "Commands".into(), items, filter: String::new(), selected: 0 }));
    }

    fn pick(&mut self, kind: PickerKind, value: String) {
        match kind {
            PickerKind::Models => {
                self.model = Some(value.clone());
                if let Some(m) = self.current_model() {
                    if self.variant.as_ref().is_some_and(|v| !m.variants().contains(v)) {
                        self.variant = None;
                    }
                }
                crate::state::push_recent_model(&value);
                self.notify(format!("Model: {value}"), false);
            }
            PickerKind::Sessions => {
                if let Ok(h) = self.app.session(&value) {
                    self.handle = h;
                    self.cache.clear();
                    self.scroll = 0;
                    if let Some(m) = self.handle.with(|s| s.model.clone()) {
                        self.model = Some(m);
                    }
                    if let Some(a) = self.handle.with(|s| s.agent.clone()) {
                        if self.app.agents.contains_key(&a) {
                            self.agent = a;
                        }
                    }
                }
            }
            PickerKind::Agents => self.agent = value,
            PickerKind::Commands => {
                if let Some(cmd) = value.strip_prefix('/') {
                    let builtin = self.slash(cmd, "");
                    if !builtin {
                        self.input.set(&value);
                    }
                }
            }
        }
    }

    // ---- input handling ----

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

        if let Some(req) = self.permissions.front().cloned() {
            let reply = match k.code {
                KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Char('1') => Some(Reply::Once),
                KeyCode::Char('A') | KeyCode::Char('s') | KeyCode::Char('2') => Some(Reply::Always),
                KeyCode::Esc | KeyCode::Char('r') | KeyCode::Char('d') | KeyCode::Char('n') | KeyCode::Char('3') => Some(Reply::Reject),
                _ => None,
            };
            if let Some(r) = reply {
                self.permissions.pop_front();
                self.app.reply_permission(&req.id, r);
            }
            return;
        }

        if let Some(dialog) = self.dialog.as_mut() {
            match dialog {
                Dialog::Help => self.dialog = None,
                Dialog::Picker(p) => match k.code {
                    KeyCode::Esc => self.dialog = None,
                    KeyCode::Up => p.selected = p.selected.saturating_sub(1),
                    KeyCode::Down => {
                        let n = p.visible().len();
                        if p.selected + 1 < n {
                            p.selected += 1;
                        }
                    }
                    KeyCode::PageUp => p.selected = p.selected.saturating_sub(10),
                    KeyCode::PageDown => p.selected = (p.selected + 10).min(p.visible().len().saturating_sub(1)),
                    KeyCode::Enter => {
                        let kind = p.kind;
                        let v = p.visible().get(p.selected).map(|i| i.value.clone());
                        self.dialog = None;
                        if let Some(v) = v {
                            self.pick(kind, v);
                        }
                    }
                    KeyCode::Char('d') if ctrl && p.kind == PickerKind::Sessions => {
                        if let Some(v) = p.visible().get(p.selected).map(|i| i.value.clone()) {
                            let _ = crate::session::delete(&self.app.project, &v);
                            self.app.forget_session(&v);
                            p.items.retain(|i| i.value != v);
                            p.selected = p.selected.min(p.visible().len().saturating_sub(1));
                            if v == self.handle.id() {
                                self.handle = self.app.new_session(None);
                                self.cache.clear();
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        p.filter.pop();
                        p.selected = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        p.filter.push(c);
                        p.selected = 0;
                    }
                    _ => {}
                },
            }
            return;
        }

        match k.code {
            KeyCode::Char('c') if ctrl => {
                if !self.input.text.is_empty() {
                    self.input.clear();
                } else if self.handle.is_busy() && self.last_ctrl_c.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                    self.handle.abort();
                    self.last_ctrl_c = Some(Instant::now());
                    self.notify("Interrupted. Press ctrl+c again to quit.", false);
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.text.is_empty() {
                    self.quit = true;
                } else {
                    self.input.delete();
                }
            }
            KeyCode::Esc => {
                if self.handle.is_busy() {
                    self.handle.abort();
                    self.app.reject_all_pending(&self.sid());
                    self.notify("Interrupted", false);
                }
            }
            KeyCode::Char('p') if ctrl => self.open_commands(),
            KeyCode::Char('n') if ctrl => self.new_session(),
            KeyCode::Char('l') if ctrl => self.open_sessions(),
            KeyCode::Char('o') if ctrl => self.open_models(),
            KeyCode::Char('t') if ctrl => self.cycle_variant(),
            KeyCode::Char('x') if ctrl => self.toggle_details(),
            KeyCode::Char('r') if ctrl => self.toggle_thinking(),
            KeyCode::F(2) => self.open_models(),
            KeyCode::Tab => self.cycle_agent(false),
            KeyCode::BackTab => self.cycle_agent(true),
            KeyCode::Enter if shift || alt => self.input.insert('\n'),
            KeyCode::Char('j') if ctrl => self.input.insert('\n'),
            KeyCode::Enter => {
                if self.input.text.ends_with('\\') {
                    self.input.backspace();
                    self.input.insert('\n');
                } else {
                    self.submit();
                }
            }
            KeyCode::Backspace if alt || ctrl => self.input.delete_word(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('u') if ctrl => {
                let s = self.input.line_start();
                self.input.text.replace_range(s..self.input.cursor, "");
                self.input.cursor = s;
            }
            KeyCode::Up => {
                if !self.input.up() && !self.history.is_empty() {
                    let i = match self.history_idx {
                        None => self.history.len() - 1,
                        Some(i) => i.saturating_sub(1),
                    };
                    self.history_idx = Some(i);
                    let h = self.history[i].clone();
                    self.input.set(&h);
                }
            }
            KeyCode::Down => {
                if !self.input.down() {
                    if let Some(i) = self.history_idx {
                        if i + 1 < self.history.len() {
                            self.history_idx = Some(i + 1);
                            let h = self.history[i + 1].clone();
                            self.input.set(&h);
                        } else {
                            self.history_idx = None;
                            self.input.clear();
                        }
                    }
                }
            }
            KeyCode::PageUp => self.scroll += self.body_height.saturating_sub(2).max(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.body_height.saturating_sub(2).max(1)),
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            _ => {}
        }
    }

    fn on_app_event(&mut self, ev: AppEvent) {
        let sid = self.sid();
        match ev {
            AppEvent::Permission(req) if req.session_id == sid || self.is_child(&req.session_id) => self.permissions.push_back(req),
            AppEvent::PermissionResolved { id } => self.permissions.retain(|r| r.id != id),
            AppEvent::Error { session, message } if session == sid => self.notify(message, true),
            AppEvent::Retry { session, message } if session == sid => self.retry = Some(message),
            AppEvent::Busy { session, busy: false } if session == sid => self.retry = None,
            AppEvent::TextDelta { session, .. } if session == sid => self.retry = None,
            AppEvent::Notice { message } => self.notify(message, false),
            _ => {}
        }
    }

    fn is_child(&self, session: &str) -> bool {
        self.app.session(session).map(|h| h.with(|s| s.parent_id.clone())).ok().flatten().is_some()
    }

    // ---- drawing ----

    fn conversation_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        let opts = render::RenderOpts { width, thinking: self.thinking, details: self.details, spinner: SPINNER[self.frame % SPINNER.len()] };
        let handle = self.handle.clone();
        let busy = handle.is_busy();
        let cache = &mut self.cache;
        let mut out = Vec::new();
        let s = handle.session.lock().unwrap();
        let last_id = s.messages.last().map(|m| m.id.clone());
        for m in &s.messages {
            let key = message_key(m);
            let live = Some(&m.id) == last_id.as_ref() && busy;
            if !live {
                if let Some((k, w, lines)) = cache.get(&m.id) {
                    if *k == key && *w == width {
                        out.extend(lines.iter().cloned());
                        continue;
                    }
                }
            }
            let lines = render::message(m, &opts);
            if !live {
                cache.insert(m.id.clone(), (key, width, lines.clone()));
            }
            out.extend(lines);
        }
        drop(s);
        // queued messages (steering)
        let queued: Vec<String> = self.handle.queue.lock().unwrap().iter().map(|(m, _)| m.text()).collect();
        for q in queued {
            out.push(Line::from(vec![Span::styled("┃ ", Style::default().fg(THEME.muted)), Span::styled(format!("(queued) {}", crate::util::truncate_chars(&q, width.saturating_sub(12))), Style::default().fg(THEME.muted))]));
        }
        out
    }

    fn welcome(&self, area: Rect, f: &mut Frame) {
        let logo = [
            "█▀▀█ █▀▀█ █▀▀ █▀▀▄ █▀▀ █▀▀█ █▀▀▄ █▀▀",
            "█  █ █  █ █▀▀ █  █ █   █  █ █  █ █▀▀",
            "▀▀▀▀ █▀▀▀ ▀▀▀ ▀  ▀ ▀▀▀ ▀▀▀▀ ▀▀▀  ▀▀▀",
        ];
        let mut lines: Vec<Line> = logo.iter().map(|l| Line::from(Span::styled(*l, Style::default().fg(THEME.accent)))).collect();
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(format!("v{}  ·  {}", crate::VERSION, self.app.directory.display()), Style::default().fg(THEME.muted))));
        lines.push(Line::default());
        for (k, d) in [("/models", "switch model"), ("/sessions", "resume a session"), ("/init", "create AGENTS.md"), ("tab", "switch agent"), ("ctrl+p", "commands"), ("/help", "all keybinds")] {
            lines.push(Line::from(vec![Span::styled(format!("{k:>10}  "), Style::default().fg(THEME.text).add_modifier(Modifier::BOLD)), Span::styled(format!("{d:<18}"), Style::default().fg(THEME.muted))]));
        }
        let h = lines.len() as u16;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let r = Rect { x: area.x, y, width: area.width, height: h.min(area.height) };
        f.render_widget(Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center), r);
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let input_width = area.width.saturating_sub(4) as usize;
        let (rows, cursor) = self.input.layout(input_width.max(1));
        let input_h = (rows.len().clamp(1, 8) + 2) as u16;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(input_h), Constraint::Length(1)])
            .split(area);

        // header
        let (title, usage_line) = self.handle.with(|s| {
            let (u, cost) = s.usage_total();
            let _ = u;
            let ctx = s.last_assistant().map(|m| m.usage.total_context()).unwrap_or(0);
            (s.title.clone(), (ctx, cost))
        });
        let ctx_info = match (self.current_model(), usage_line.0) {
            (_, 0) => String::new(),
            (Some(m), c) if m.context > 0 => format!("{}  {}%", fmt_tokens(c), c * 100 / m.context),
            (_, c) => fmt_tokens(c),
        };
        let right = format!("{ctx_info}{}", if usage_line.1 > 0.0 { format!("  ${:.2}", usage_line.1) } else { String::new() });
        let left = format!(" {}", crate::util::truncate_chars(&title, (area.width as usize).saturating_sub(right.width() + 4)));
        let pad = (area.width as usize).saturating_sub(left.width() + right.width() + 1);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(left, Style::default().add_modifier(Modifier::BOLD)), Span::raw(" ".repeat(pad)), Span::styled(right, Style::default().fg(THEME.muted))])),
            chunks[0],
        );

        // body
        let body = chunks[1];
        let width = body.width.saturating_sub(2) as usize;
        self.body_height = body.height as usize;
        let lines = self.conversation_lines(width);
        if lines.is_empty() {
            self.welcome(body, f);
        } else {
            let h = body.height as usize;
            let total = lines.len();
            let max_scroll = total.saturating_sub(h);
            self.scroll = self.scroll.min(max_scroll);
            let end = total - self.scroll;
            let start = end.saturating_sub(h);
            let visible: Vec<Line> = lines[start..end].to_vec();
            let inner = Rect { x: body.x + 1, y: body.y, width: body.width.saturating_sub(2), height: body.height };
            f.render_widget(Paragraph::new(visible), inner);
            if self.scroll > 0 {
                let tag = format!(" ↓ {} more ", self.scroll);
                let r = Rect { x: body.x + body.width.saturating_sub(tag.width() as u16 + 1), y: body.y + body.height - 1, width: tag.width() as u16, height: 1 };
                f.render_widget(Paragraph::new(Span::styled(tag, Style::default().fg(THEME.accent))), r);
            }
        }

        // status line
        let busy = self.handle.is_busy();
        let status = if let Some(r) = &self.retry {
            Line::from(Span::styled(format!(" {} {}", SPINNER[self.frame % SPINNER.len()], crate::util::truncate_chars(r, area.width as usize - 4)), Style::default().fg(THEME.warning)))
        } else if let Some((msg, at, err)) = &self.notice {
            if at.elapsed() < Duration::from_secs(if *err { 12 } else { 4 }) {
                Line::from(Span::styled(format!(" {}", crate::util::truncate_chars(msg, area.width as usize - 2)), Style::default().fg(if *err { THEME.error } else { THEME.muted })))
            } else if busy {
                working_line(self.frame)
            } else {
                Line::default()
            }
        } else if busy {
            working_line(self.frame)
        } else {
            Line::default()
        };
        f.render_widget(Paragraph::new(status), chunks[2]);

        // input
        let agent_color = self.app.agents.get(&self.agent).and_then(|a| a.color.as_deref()).and_then(parse_color).unwrap_or(THEME.accent);
        let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(if busy { THEME.muted } else { agent_color }));
        let inner = block.inner(chunks[3]);
        f.render_widget(block, chunks[3]);
        let visible_rows = inner.height as usize;
        let first = cursor.0.saturating_sub(visible_rows.saturating_sub(1));
        let text: Vec<Line> = if self.input.text.is_empty() {
            vec![Line::from(Span::styled(if busy { "Type to queue a message (esc to interrupt)" } else { "Ask anything… (@file to attach, /command)" }, Style::default().fg(THEME.muted)))]
        } else {
            rows.iter().skip(first).take(visible_rows).map(|r| Line::from(r.clone())).collect()
        };
        let input_area = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
        f.render_widget(Paragraph::new(text), input_area);
        if self.dialog.is_none() && self.permissions.is_empty() {
            f.set_cursor_position((input_area.x + cursor.1 as u16, input_area.y + (cursor.0 - first) as u16));
        }

        // footer
        let model = self.model_ref().unwrap_or_else(|| "no model".into());
        let agent_name = self.app.agents.get(&self.agent).map(|a| a.name.clone()).unwrap_or_else(|| self.agent.clone());
        let hints = "tab agent · ctrl+p commands · ctrl+o model · esc interrupt";
        let left = vec![Span::styled(format!(" {agent_name} "), Style::default().fg(agent_color).add_modifier(Modifier::BOLD)), Span::styled(format!(" {model}"), Style::default().fg(THEME.muted))];
        let lw: usize = left.iter().map(|s| s.content.width()).sum();
        let mut spans = left;
        if area.width as usize > lw + hints.len() + 2 {
            spans.push(Span::raw(" ".repeat(area.width as usize - lw - hints.len() - 1)));
            spans.push(Span::styled(hints, Style::default().fg(THEME.muted)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), chunks[4]);

        // overlays
        if let Some(req) = self.permissions.front() {
            draw_permission(f, area, req);
        } else if let Some(d) = &self.dialog {
            match d {
                Dialog::Picker(p) => draw_picker(f, area, p),
                Dialog::Help => draw_help(f, area),
            }
        }
    }

    async fn run(mut self, terminal: &mut DefaultTerminal, initial: Option<String>) -> Result<()> {
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || loop {
            match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        });
        let mut events = self.app.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(80));
        if let Some(p) = initial {
            self.input.set(&p);
            self.submit();
        }
        let mut dirty = true;
        loop {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }
            tokio::select! {
                Some(ev) = keys.recv() => {
                    match ev {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Paste(s) => self.input.insert_str(&s),
                        Event::Mouse(m) => match m.kind {
                            MouseEventKind::ScrollUp => self.scroll += 3,
                            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                            _ => {}
                        },
                        _ => {}
                    }
                    // drain bursts (e.g. pasted text without bracketed paste)
                    while let Ok(ev) = keys.try_recv() {
                        match ev {
                            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                            Event::Paste(s) => self.input.insert_str(&s),
                            _ => {}
                        }
                    }
                    dirty = true;
                }
                ev = events.recv() => {
                    match ev {
                        Ok(ev) => self.on_app_event(ev),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => {}
                    }
                    dirty = true;
                }
                _ = tick.tick() => {
                    if self.handle.is_busy() || self.notice.is_some() {
                        self.frame += 1;
                        dirty = true;
                    }
                }
            }
            if self.quit {
                break;
            }
        }
        self.handle.abort();
        Ok(())
    }
}

fn working_line(frame: usize) -> Line<'static> {
    Line::from(vec![Span::styled(format!(" {} ", SPINNER[frame % SPINNER.len()]), Style::default().fg(THEME.accent)), Span::styled("Working…  esc to interrupt", Style::default().fg(THEME.muted))])
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn parse_color(s: &str) -> Option<ratatui::style::Color> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some(ratatui::style::Color::Rgb(p(0)?, p(2)?, p(4)?))
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_picker(f: &mut Frame, area: Rect, p: &Picker) {
    let r = centered(area, 90, area.height.saturating_sub(6).min(24));
    f.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(format!(" {} ", p.title)).border_style(Style::default().fg(THEME.accent));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines = vec![Line::from(vec![Span::styled("> ", Style::default().fg(THEME.accent)), Span::raw(p.filter.clone())]), Line::default()];
    let items = p.visible();
    let rows = inner.height.saturating_sub(2) as usize;
    let start = p.selected.saturating_sub(rows.saturating_sub(1));
    let w = inner.width as usize;
    for (i, it) in items.iter().enumerate().skip(start).take(rows) {
        let sel = i == p.selected;
        let label = crate::util::truncate_chars(&it.label, w.saturating_sub(4) * 3 / 5);
        let room = w.saturating_sub(label.width() + 4);
        let detail = crate::util::truncate_chars(&it.detail, room);
        let pad = w.saturating_sub(label.width() + detail.width() + 3);
        let style = if sel { Style::default().fg(ratatui::style::Color::Black).bg(THEME.accent) } else { Style::default() };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label}"), style.add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(" ".repeat(pad), style),
            Span::styled(format!("{detail} "), if sel { style } else { Style::default().fg(THEME.muted) }),
        ]));
    }
    if items.is_empty() {
        lines.push(Line::from(Span::styled(" no matches", Style::default().fg(THEME.muted))));
    }
    f.render_widget(Paragraph::new(lines), inner);
    f.set_cursor_position((inner.x + 2 + p.filter.width() as u16, inner.y));
}

fn draw_permission(f: &mut Frame, area: Rect, req: &permission::Request) {
    let diff = req.metadata.get("diff").and_then(|d| d.as_str()).unwrap_or("");
    let diff_lines: Vec<&str> = diff.lines().filter(|l| !l.starts_with("---") && !l.starts_with("+++")).take(16).collect();
    let h = (8 + diff_lines.len() + req.resources.len().min(4)) as u16;
    let r = centered(area, 100, h);
    f.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" Permission required ").border_style(Style::default().fg(THEME.warning)).padding(Padding::horizontal(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let mut lines = vec![Line::from(Span::styled(crate::util::truncate_chars(&req.title, w), Style::default().add_modifier(Modifier::BOLD))), Line::default()];
    lines.push(Line::from(vec![Span::styled("action  ", Style::default().fg(THEME.muted)), Span::raw(req.action.clone())]));
    for res in req.resources.iter().take(4) {
        lines.push(Line::from(vec![Span::styled("target  ", Style::default().fg(THEME.muted)), Span::raw(crate::util::truncate_chars(res, w.saturating_sub(8)))]));
    }
    if !diff_lines.is_empty() {
        lines.push(Line::default());
        for l in diff_lines {
            let c = if l.starts_with('+') { THEME.success } else if l.starts_with('-') { THEME.error } else { THEME.muted };
            lines.push(Line::from(Span::styled(crate::util::truncate_chars(l, w), Style::default().fg(c))));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" a/enter ", Style::default().fg(ratatui::style::Color::Black).bg(THEME.success)),
        Span::raw(" allow once   "),
        Span::styled(" s ", Style::default().fg(ratatui::style::Color::Black).bg(THEME.accent)),
        Span::raw(" always allow   "),
        Span::styled(" r/esc ", Style::default().fg(ratatui::style::Color::Black).bg(THEME.error)),
        Span::raw(" reject"),
    ]));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows = [
        ("enter", "send message"),
        ("shift+enter / ctrl+j", "new line"),
        ("esc", "interrupt the agent"),
        ("tab / shift+tab", "cycle agents"),
        ("ctrl+p", "command palette"),
        ("ctrl+o / F2", "switch model"),
        ("ctrl+t", "cycle model variant (reasoning effort)"),
        ("ctrl+n", "new session"),
        ("ctrl+l", "switch session"),
        ("ctrl+r", "toggle thinking blocks"),
        ("ctrl+x", "toggle tool details"),
        ("pgup / pgdn / wheel", "scroll conversation"),
        ("up / down", "prompt history"),
        ("@path", "attach a file or directory"),
        ("/init /review /compact /export", "built-in commands"),
        ("ctrl+c", "clear input / interrupt / quit"),
    ];
    let r = centered(area, 72, rows.len() as u16 + 4);
    f.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" Help ").border_style(Style::default().fg(THEME.accent)).padding(Padding::horizontal(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| Line::from(vec![Span::styled(format!(" {k:<32}"), Style::default().add_modifier(Modifier::BOLD)), Span::styled(*d, Style::default().fg(THEME.muted))]))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

pub async fn start(dir: std::path::PathBuf, opts: StartOptions) -> Result<()> {
    let app = App::init(dir).await?;
    let ui = Ui::new(app.clone(), &opts)?;
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let res = ui.run(&mut terminal, opts.prompt.clone()).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Part, ToolState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn dump(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[tokio::test]
    async fn draws_conversation_and_overlays() {
        let dir = std::env::temp_dir().join("opencode-tui-test");
        std::fs::create_dir_all(&dir).unwrap();
        let app = App::init(dir).await.unwrap();
        let opts = StartOptions { model: Some("anthropic/claude-sonnet-5-5".into()), agent: None, continue_last: false, session: None, fork: false, prompt: None };
        let mut ui = Ui::new(app, &opts).unwrap();
        ui.thinking = true;
        ui.handle.with(|s| {
            s.title = "Fix notes file".into();
            s.messages.push(Message::user_text("Please fix notes.txt and explain"));
            let mut a = Message::new(Role::Assistant);
            a.parts.push(Part::Reasoning { text: "I should read the file first.".into(), signature: None, redacted: None, item_id: None });
            a.parts.push(Part::Tool { id: "1".into(), name: "shell".into(), input: json!({ "command": "git status" }), state: ToolState::Completed { output: "On branch main\nnothing to commit".into(), title: "git status".into(), metadata: json!({}), attachments: vec![] }, provider_meta: None });
            let diff = crate::util::unified_diff("notes.txt", "hello\nsecond line\n", "hello\nline two\n");
            a.parts.push(Part::Tool { id: "2".into(), name: "edit".into(), input: json!({ "path": "notes.txt" }), state: ToolState::Completed { output: "Edited".into(), title: "notes.txt".into(), metadata: json!({ "files": [{ "file": "notes.txt", "diff": diff, "additions": 1, "deletions": 1 }] }), attachments: vec![] }, provider_meta: None });
            a.parts.push(Part::Tool { id: "3".into(), name: "read".into(), input: json!({ "path": "missing.txt" }), state: ToolState::Error { error: "File not found: missing.txt".into() }, provider_meta: None });
            a.parts.push(Part::Text { text: "## Summary\n\nI changed **one line**:\n\n- `second line` → `line two`\n- kept the rest\n\n```rust\nfn main() {}\n```".into(), synthetic: false });
            s.messages.push(a);
        });
        let mut t = Terminal::new(TestBackend::new(90, 40)).unwrap();
        t.draw(|f| ui.draw(f)).unwrap();
        let screen = dump(&t);
        println!("{screen}");
        for needle in ["Fix notes file", "Please fix notes.txt", "Thinking:", "shell", "git status", "nothing to commit", "notes.txt +1 -1", "-second line", "+line two", "File not found", "Summary", "• second line → line two", "fn main() {}", "Build", "claude-sonnet-5-5"] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }

        ui.permissions.push_back(permission::Request {
            id: "per_1".into(),
            session_id: ui.sid(),
            tool_call_id: None,
            action: "shell".into(),
            resources: vec!["rm -rf build".into()],
            title: "Run: rm -rf build".into(),
            metadata: json!({}),
            save: vec!["rm *".into()],
        });
        t.draw(|f| ui.draw(f)).unwrap();
        let screen = dump(&t);
        println!("{screen}");
        assert!(screen.contains("Permission required") && screen.contains("rm -rf build") && screen.contains("always allow"));

        ui.permissions.clear();
        ui.open_commands();
        t.draw(|f| ui.draw(f)).unwrap();
        let screen = dump(&t);
        println!("{screen}");
        assert!(screen.contains("Commands") && screen.contains("New session") && screen.contains("/init"));

        // empty session shows the welcome screen
        ui.dialog = None;
        ui.handle = ui.app.new_session(None);
        t.draw(|f| ui.draw(f)).unwrap();
        let screen = dump(&t);
        println!("{screen}");
        assert!(screen.contains("/models"));
    }

    #[test]
    fn input_editing() {
        let mut i = Input::default();
        i.insert_str("hello world");
        i.delete_word();
        assert_eq!(i.text, "hello ");
        i.insert_str("there\nsecond");
        assert!(i.up());
        assert_eq!(i.cursor, 6);
        i.end();
        i.backspace();
        assert_eq!(i.text, "hello ther\nsecond");
        let (rows, cur) = i.layout(4);
        assert_eq!(rows[0], "hell");
        assert_eq!(cur, (2, 2));
    }
}
