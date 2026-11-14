//! Interactive terminal UI, laid out after the TypeScript client's full-screen TUI
//! (`packages/tui`): home screen, session view, composer, dialogs, toasts and sidebar.

mod actions;
pub mod dialog;
mod diff_view;
pub mod draw;
pub mod forms;
pub mod highlight;
pub mod image_view;
pub mod keymap;
pub mod markdown;
mod navigation;
pub mod plugin_ui;
pub mod prompt;
mod reports;
pub mod terminal;
pub mod render;
pub mod settings;
mod tabs;
pub mod theme;
pub mod transcript;

use crate::app::{App, AppEvent, Handle, PromptOptions};
use crate::permission::{self, Reply};
use crate::session::{Message, Part, Role};
use anyhow::Result;
use dialog::{Dialog, Hint, Kind, Opt, Select, Size};
use draw::{line, span};
use keymap::{Key, Keymap};
use prompt::{Auto, AutoKind, Editor, History, Pick};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};
use settings::Settings;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::{Mode, Rgba, Theme};

pub struct StartOptions {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub continue_last: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub prompt: Option<String>,
    pub mini: bool,
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(40);
/// Messages rendered when a long session opens; older chunks load on scroll.
const TRANSCRIPT_WINDOW: usize = 250;
const LEADER_TIMEOUT: Duration = Duration::from_millis(2000);
const LOGO_LEFT: [&str; 4] = ["                   ", "█▀▀█ █▀▀█ █▀▀█ █▀▀▄", "█__█ █__█ █^^^ █__█", "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀"];
const LOGO_RIGHT: [&str; 4] = ["             ▄     ", "█▀▀▀ █▀▀█ █▀▀█ █▀▀█", "█___ █__█ █__█ █^^^", "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀"];

// ---------------------------------------------------------------- model preferences

/// `<state>/model.json`, shared with the TypeScript client: recent, favorite and per-model variant.
#[derive(Default)]
struct ModelPrefs {
    recent: Vec<String>,
    favorite: Vec<String>,
    variant: serde_json::Map<String, Value>,
}

fn prefs_path() -> std::path::PathBuf {
    crate::paths::get().state.join("model.json")
}

impl ModelPrefs {
    fn load() -> ModelPrefs {
        let v: Value = std::fs::read_to_string(prefs_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        let list = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|m| Some(format!("{}/{}", m.get("providerID")?.as_str()?, m.get("modelID")?.as_str()?))).collect())
                .unwrap_or_default()
        };
        ModelPrefs { recent: list("recent"), favorite: list("favorite"), variant: v.get("variant").and_then(|x| x.as_object()).cloned().unwrap_or_default() }
    }
    fn save(&self) {
        let enc = |l: &[String]| -> Vec<Value> { l.iter().filter_map(|m| m.split_once('/')).map(|(p, id)| json!({ "providerID": p, "modelID": id })).collect() };
        let v = json!({ "recent": enc(&self.recent), "favorite": enc(&self.favorite), "variant": self.variant });
        if let Some(d) = prefs_path().parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(prefs_path(), v.to_string());
    }
    fn use_model(&mut self, m: &str) {
        self.recent.retain(|x| x != m);
        self.recent.insert(0, m.to_string());
        self.recent.truncate(10);
        self.save();
    }
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Home,
    Session,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Info,
    Success,
    Warning,
    Error,
}

const TOAST_DURATION: Duration = Duration::from_millis(5000);

/// Tab commands, available on both routes.
const TAB_COMMANDS: [&str; 14] = [
    "session.tab.next",
    "session.tab.previous",
    "session.tab.close",
    "session.tab.reopen",
    "session.tab.select.1",
    "session.tab.select.2",
    "session.tab.select.3",
    "session.tab.select.4",
    "session.tab.select.5",
    "session.tab.select.6",
    "session.tab.select.7",
    "session.tab.select.8",
    "session.tab.select.9",
    "session.tab.select.10",
];

struct Toast {
    title: Option<String>,
    message: String,
    variant: Variant,
    until: Instant,
}

struct Command {
    id: String,
    title: String,
    category: String,
    slash: Vec<String>,
    suggested: bool,
    palette: bool,
}

#[derive(Default)]
struct PermUi {
    selected: usize,
    expanded: bool,
    rejecting: Option<Editor>,
}

#[derive(Default)]
struct Hits {
    transcript: Rect,
    transcript_skip: usize,
    clicks: Vec<(usize, usize, String)>,
    images: Vec<(usize, usize, String, String)>,
    dialog: Option<dialog::Placed>,
    tabs: Vec<(u16, u16, String)>,
    tabs_vertical: Vec<(Rect, String)>,
    tab_close: Vec<(u16, u16, String)>,
    jump: Option<Rect>,
    auto: Vec<(u16, usize)>,
    perm_options: Vec<(Rect, usize)>,
    terminal_pane: Rect,
    terminal_tabs: Vec<(Rect, usize)>,
    terminal_close: Vec<(Rect, usize)>,
    composer_panel: Rect,
    composer_tabs: Vec<(Rect, usize)>,
    composer_rows: Vec<(Rect, usize)>,
}

struct Cache {
    /// Width, theme, settings, queue and other renderer inputs.
    key: u64,
    out: transcript::Out,
    /// First message rendered; older messages load when the user scrolls up.
    start: usize,
    /// Whether the previous chunk above the window has already been loaded.
    older_loaded: bool,
    /// Busy state used by the live suffix footer.
    busy: bool,
    /// Number of messages in the cached prefix, and its fingerprint.
    prefix_len: usize,
    prefix_key: u64,
    /// Line/click/user counts at the end of the prefix, so the live suffix can
    /// be replaced without rebuilding the whole transcript.
    base_lines: usize,
    base_clicks: usize,
    base_users: usize,
    last_key: u64,
    last_end_lines: usize,
    last_end_clicks: usize,
    last_end_users: usize,
}

enum Act {
    None,
    Close,
    Pick(Kind, String),
    Rename(String, String),
    RenameOpen(String, String),
    McpAuthOpen(String),
    McpAuthCopy(String),
    McpAuthSubmit(String),
    Sessions(bool),
    Delete(String),
    Favorite(String, usize),
    Provider,
}

#[derive(Clone)]
struct Stashed {
    text: String,
    pasted: Vec<(String, String)>,
    shell: bool,
}

struct Ui {
    app: Arc<App>,
    handle: Arc<Handle>,
    route: Route,
    settings: Settings,
    mini: bool,
    keys: Keymap,
    theme: Theme,
    mode: Mode,
    editor: Editor,
    history: History,
    stash: Vec<Stashed>,
    pasted: Vec<(String, String)>,
    shell: bool,
    placeholder: usize,
    auto: Option<Auto>,
    agent: String,
    model: Option<String>,
    variant: Option<String>,
    prefs: ModelPrefs,
    dialog: Option<Dialog>,
    permissions: VecDeque<permission::Request>,
    perm: PermUi,
    form: forms::FormUi,
    toasts: VecDeque<Toast>,
    scroll: usize,
    expanded: HashSet<String>,
    sidebar: Option<bool>,
    frame: usize,
    leader: Option<Instant>,
    interrupt: Option<Instant>,
    tabs: Vec<String>,
    /// tabs as last written to disk
    saved_tabs: Vec<String>,
    /// closed tabs with their former position, most recent last
    closed_tabs: Vec<(String, usize)>,
    /// tab under the mouse pointer; the close mark only shows while hovering
    tab_hover: Option<String>,
    /// when the current tab hover began, for the delayed title marquee
    tab_hover_at: Option<Instant>,
    /// sessions with activity since they were last viewed
    tab_unread: HashSet<String>,
    /// tab being dragged and the slot it was picked up from
    tab_drag: Option<(String, usize)>,
    tab_drag_moved: bool,
    /// session targeted by the tab context menu
    tab_menu_target: Option<String>,
    /// sessions in visit order, most recent last
    tab_history: Vec<String>,
    /// terminal title and default background as last sent (OSC 0 / OSC 11)
    chrome: (String, String),
    redo: Vec<(String, Vec<Message>)>,
    shells: Vec<String>,
    terminal_pane: terminal::TerminalPane,
    terminal_resizing: bool,
    composer_open: bool,
    composer_tab: usize,
    composer_selected: usize,
    hits: Hits,
    cache: Option<Cache>,
    body_height: usize,
    quit: bool,
    size: (u16, u16),
    theme_before: Option<String>,
    pending_editor: bool,
    plugin_status: String,
    plugin_sidebar: Vec<Value>,
    plugin_slots_at: Instant,
}

/// Hovered tab title: after 600 ms, advance one cell every 80 ms through
/// `title · title · ` and take the window that fits.
fn tab_marquee(title: &str, width: usize, elapsed: Option<Duration>) -> String {
    if width == 0 {
        return String::new();
    }
    let Some(elapsed) = elapsed else {
        return draw::take_width(title, width);
    };
    if draw::width(title) <= width || elapsed < Duration::from_millis(600) {
        return draw::take_width(title, width);
    }
    let cycle = format!("{title} · ");
    let cycle_width = draw::width(&cycle);
    if cycle_width == 0 {
        return String::new();
    }
    let step = ((elapsed.as_millis().saturating_sub(600) / 80) as usize + 1) % cycle_width;
    let repeated = format!("{cycle}{cycle}");
    let mut out = String::new();
    let mut skipped = 0usize;
    let mut used = 0usize;
    for ch in repeated.chars() {
        let cw = draw::width(&ch.to_string());
        if skipped + cw <= step {
            skipped += cw;
            continue;
        }
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

fn titlecase(s: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for c in s.chars() {
        if start && c.is_alphanumeric() {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        start = !(c.is_alphanumeric() || c == '_');
    }
    out
}

fn git_branch(dir: &std::path::Path) -> Option<String> {
    let mut d = Some(dir);
    while let Some(p) = d {
        let git = p.join(".git");
        let head = if git.is_dir() {
            std::fs::read_to_string(git.join("HEAD")).ok()
        } else if git.is_file() {
            let link = std::fs::read_to_string(&git).ok()?;
            let gd = link.trim().strip_prefix("gitdir: ")?.to_string();
            std::fs::read_to_string(std::path::Path::new(&gd).join("HEAD")).ok()
        } else {
            None
        };
        if let Some(h) = head {
            return h.trim().strip_prefix("ref: refs/heads/").map(String::from).or_else(|| Some(h.trim().chars().take(7).collect()));
        }
        d = p.parent();
    }
    None
}

fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let _ = std::io::Write::write_all(&mut std::io::stdout(), format!("\x1b]52;c;{b64}\x07").as_bytes());
    #[cfg(windows)]
    {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("clip").stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }).arg(url).spawn();
}

fn layout_text(node: &Value) -> String {
    match node.get("type").and_then(Value::as_str) {
        Some("text") => node.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        Some("box") => node
            .get("children")
            .and_then(Value::as_array)
            .map(|children| children.iter().map(layout_text).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("  "))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn layout_plain_lines(node: &Value, depth: usize) -> Vec<String> {
    if depth > 8 {
        return Vec::new();
    }
    match node.get("type").and_then(Value::as_str) {
        Some("text") => node
            .get("text")
            .and_then(Value::as_str)
            .map(|text| vec![text.to_string()])
            .unwrap_or_default(),
        Some("box") => {
            let children = node.get("children").and_then(Value::as_array).cloned().unwrap_or_default();
            if node.get("direction").and_then(Value::as_str) == Some("row") {
                let line = children.iter().map(layout_text).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("  ");
                if line.is_empty() { Vec::new() } else { vec![line] }
            } else {
                children.iter().flat_map(|child| layout_plain_lines(child, depth + 1)).collect()
            }
        }
        _ => Vec::new(),
    }
}

impl Ui {
    fn new(app: Arc<App>, opts: &StartOptions) -> Result<Ui> {
        let settings = Settings::load();
        let keys = Keymap::new(&settings.keybinds());
        let mode = match settings.str("theme.mode") {
            Some("light") => Mode::Light,
            _ => Mode::Dark,
        };
        let mut route = Route::Home;
        let handle = if let Some(id) = &opts.session {
            route = Route::Session;
            app.session(id).unwrap_or_else(|_| app.new_session(None))
        } else if opts.continue_last {
            match crate::session::list(&app.project).into_iter().find(|s| s.parent_id.is_none()) {
                Some(s) => {
                    route = Route::Session;
                    app.session(&s.id)?
                }
                None => app.new_session(None),
            }
        } else {
            app.new_session(None)
        };
        let handle = if opts.fork && route == Route::Session {
            let f = crate::session::fork(&handle.session.lock().unwrap());
            crate::session::save(&f)?;
            app.session(&f.id)?
        } else {
            handle
        };
        let agent = opts.agent.clone().or_else(|| handle.with(|s| s.agent.clone())).filter(|a| app.agents().contains_key(a)).unwrap_or_else(|| app.default_agent());
        let mut ui = Ui {
            theme: theme::fallback(mode),
            app,
            handle,
            route,
            settings,
            mini: opts.mini,
            keys,
            mode,
            editor: Editor::default(),
            history: History::load(),
            stash: Vec::new(),
            pasted: vec![],
            shell: false,
            placeholder: (crate::util::now_ms() as usize) % 3,
            auto: None,
            agent,
            model: None,
            variant: None,
            prefs: ModelPrefs::load(),
            dialog: None,
            permissions: VecDeque::new(),
            perm: PermUi::default(),
            form: forms::FormUi::default(),
            toasts: VecDeque::new(),
            scroll: 0,
            expanded: HashSet::new(),
            sidebar: None,
            frame: 0,
            leader: None,
            interrupt: None,
            tabs: vec![],
            saved_tabs: vec![],
            closed_tabs: vec![],
            tab_hover: None,
            tab_hover_at: None,
            tab_unread: HashSet::new(),
            tab_drag: None,
            tab_drag_moved: false,
            tab_menu_target: None,
            tab_history: vec![],
            chrome: Default::default(),
            redo: vec![],
            shells: vec![],
            terminal_pane: terminal::TerminalPane::default(),
            terminal_resizing: false,
            composer_open: false,
            composer_tab: 0,
            composer_selected: 0,
            hits: Hits::default(),
            cache: None,
            body_height: 20,
            quit: false,
            size: (80, 24),
            theme_before: None,
            pending_editor: false,
            plugin_status: String::new(),
            plugin_sidebar: Vec::new(),
            plugin_slots_at: Instant::now(),
        };
        let name = ui.settings.theme();
        ui.set_theme(&name);
        ui.publish_theme();
        ui.pick_model(opts.model.clone());
        let scope = ui.settings.tabs_scope().to_string();
        ui.tabs = tabs::load(&ui.app.directory, &scope).into_iter().filter(|id| crate::session::load(&ui.app.project, id).is_ok()).collect();
        ui.saved_tabs = ui.tabs.clone();
        if ui.route == Route::Session {
            let id = ui.sid();
            ui.visit(&id);
        }
        Ok(ui)
    }

    // ---------------------------------------------------------------- tabs

    /// Record a visit: the session gets a tab and moves to the end of the history.
    fn visit(&mut self, id: &str) {
        if !self.tabs.iter().any(|t| t == id) {
            self.tabs.push(id.to_string());
        }
        self.tab_history.retain(|t| t != id);
        self.tab_history.push(id.to_string());
    }

    fn persist_tabs(&mut self) {
        if self.tabs == self.saved_tabs {
            return;
        }
        let list: Vec<(String, String)> = self
            .tabs
            .iter()
            .map(|id| {
                let title = self.app.session(id).map(|h| h.with(|s| s.title.clone())).unwrap_or_default();
                (id.clone(), title)
            })
            .collect();
        let scope = if self.settings.str("tabs.scope") == Some("global") { "global" } else { "cwd" };
        tabs::save(&self.app.directory, scope, &list);
        self.saved_tabs = self.tabs.clone();
    }

    /// Window title like upstream's `app.tsx` ("OpenCode", or "OC | <title>" in a titled session),
    /// and the theme background as the terminal's default colour so window padding blends in.
    fn chrome(&self) -> (String, String) {
        let title = match self.route {
            Route::Session => {
                let t = self.handle.with(|s| s.title.clone());
                let fallback = t.is_empty() || t.starts_with(crate::session::DEFAULT_TITLE_PREFIX) || t.starts_with("Child session - ");
                if fallback {
                    "OpenCode".to_string()
                } else if t.chars().count() > 40 {
                    format!("OC | {}…", t.chars().take(37).collect::<String>())
                } else {
                    format!("OC | {t}")
                }
            }
            _ => "OpenCode".to_string(),
        };
        let title = title.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let bg = if self.theme.bg.a > 0.0 { self.theme.bg.hex() } else { String::new() };
        (title, bg)
    }

    fn sync_chrome(&mut self) {
        let next = self.chrome();
        if next == self.chrome {
            return;
        }
        let mut out = String::new();
        if next.0 != self.chrome.0 {
            out += &format!("\x1b]0;{}\x07", next.0);
        }
        if next.1 != self.chrome.1 {
            out += &if next.1.is_empty() { "\x1b]111\x1b\\".to_string() } else { format!("\x1b]11;{}\x1b\\", next.1) };
        }
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes()).and_then(|_| stdout.flush());
        self.chrome = next;
    }

    /// Most recently visited session that still has a tab.
    fn last_visited_tab(&self) -> Option<String> {
        self.tab_history.iter().rev().find(|id| self.tabs.contains(id)).cloned()
    }

    /// Upstream `session.tab.close`: close the current tab and go back to the previously visited
    /// one (else a neighbour, else home). From home it returns to the last visited tab.
    fn close_tab(&mut self) {
        if self.route == Route::Home {
            if let Some(id) = self.last_visited_tab().or_else(|| self.tabs.last().cloned()) {
                self.open_session(&id);
            }
            return;
        }
        self.close_tab_id(&self.sid());
    }

    /// Close a specific tab. Closing the active tab returns to the previous visit, then a
    /// neighbour, then a fresh session; closing another tab leaves the current session alone.
    fn close_tab_id(&mut self, target: &str) {
        let target = target.to_string();
        let Some(index) = self.tabs.iter().position(|t| *t == target) else { return };
        let active = self.route == Route::Session && self.sid() == target;
        self.tabs.remove(index);
        self.closed_tabs.retain(|(id, _)| *id != target);
        self.closed_tabs.push((target.clone(), index));
        if self.closed_tabs.len() > tabs::CLOSED_LIMIT {
            self.closed_tabs.remove(0);
        }
        self.tab_history.retain(|t| *t != target);
        if !active {
            return;
        }
        let next = self.last_visited_tab().or_else(|| self.tabs.get(index).or_else(|| index.checked_sub(1).and_then(|i| self.tabs.get(i))).cloned());
        match next {
            Some(id) => self.open_session(&id),
            None => self.new_session(),
        }
    }

    /// Upstream `session.tab.reopen`: restore the most recently closed tab at its old position.
    fn reopen_tab(&mut self) {
        while let Some((id, index)) = self.closed_tabs.pop() {
            if self.tabs.contains(&id) || crate::session::load(&self.app.project, &id).is_err() {
                continue;
            }
            self.tabs.insert(index.min(self.tabs.len()), id.clone());
            self.open_session(&id);
            return;
        }
    }

    fn cycle_tab(&mut self, step: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let current = if self.route == Route::Session { self.tabs.iter().position(|t| *t == self.sid()) } else { None };
        let start = current.map(|i| i as isize).unwrap_or(if step > 0 { -1 } else { 0 });
        let id = self.tabs[(start + step).rem_euclid(n) as usize].clone();
        self.open_session(&id);
    }

    fn cycle_unread(&mut self, step: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let current = if self.route == Route::Session { self.tabs.iter().position(|t| *t == self.sid()) } else { None };
        let start = current.map(|i| i as isize).unwrap_or(if step > 0 { -1 } else { 0 });
        for offset in 1..=n {
            let index = (start + step * offset).rem_euclid(n) as usize;
            let id = &self.tabs[index];
            if self.tab_unread.contains(id) {
                let id = id.clone();
                self.open_session(&id);
                return;
            }
        }
        self.cycle_tab(step);
    }

    fn tab_history_step(&mut self, forward: bool) {
        let Some(current) = (self.route == Route::Session).then(|| self.sid()) else { return };
        let Some(index) = self.tab_history.iter().position(|id| *id == current) else { return };
        let next = if forward {
            self.tab_history.get(index + 1)
        } else {
            index.checked_sub(1).and_then(|i| self.tab_history.get(i))
        }
        .cloned();
        if let Some(id) = next.filter(|id| self.tabs.contains(id)) {
            self.open_session(&id);
        }
    }

    fn thinking_shown(&self) -> bool {
        if self.mini {
            self.settings.mini_thinking()
        } else {
            self.settings.thinking_shown()
        }
    }

    fn tabs_visible(&self) -> bool {
        if self.mini {
            return false;
        }
        match self.settings.tabs_mode() {
            "off" => false,
            "on" => true,
            _ => self.size.0 >= 64,
        }
    }

    fn tabs_vertical(&self) -> bool {
        self.tabs_visible() && self.settings.tabs_layout() == "vertical"
    }

    fn tab_at(&self, x: u16, y: u16) -> Option<String> {
        if y == 0 {
            if let Some((_, _, id)) = self.hits.tabs.iter().find(|(a, b, _)| x >= *a && x < *b) {
                return Some(id.clone());
            }
        }
        self.hits
            .tabs_vertical
            .iter()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, id)| id.clone())
    }

    fn move_tab(&mut self, source: &str, target: usize) {
        let Some(from) = self.tabs.iter().position(|id| id == source) else { return };
        let target = target.min(self.tabs.len().saturating_sub(1));
        if from == target {
            return;
        }
        let id = self.tabs.remove(from);
        self.tabs.insert(target, id);
        self.tab_drag_moved = true;
    }

    fn open_tab_menu(&mut self, id: String) {
        let title = self.app.session(&id).map(|h| h.with(|s| s.title.clone())).unwrap_or_default();
        let opts = vec![
            dialog::Opt { title: "Close tab".into(), value: "close".into(), ..Default::default() },
            dialog::Opt { title: "Close other tabs".into(), value: "close_others".into(), ..Default::default() },
            dialog::Opt { title: "Close tabs to the right".into(), value: "close_right".into(), ..Default::default() },
            dialog::Opt { title: "Reopen closed tab".into(), value: "reopen".into(), ..Default::default() },
            dialog::Opt { title: "Rename session".into(), value: "rename".into(), ..Default::default() },
        ];
        self.dialog = Some(Dialog::Select(dialog::Select::new(
            dialog::Kind::TabMenu,
            &format!("Tab: {title}"),
            opts,
        )));
        self.tab_menu_target = Some(id);
    }

    fn close_tabs_to(&mut self, target: &str, right_only: bool) {
        let Some(index) = self.tabs.iter().position(|id| id == target) else { return };
        let keep = if right_only { index + 1 } else { index + 1 };
        let mut removed = Vec::new();
        if right_only {
            removed.extend(self.tabs.drain(keep..));
        } else {
            let mut i = 0;
            while i < self.tabs.len() {
                if i != index {
                    removed.push(self.tabs.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        for id in removed {
            self.closed_tabs.push((id.clone(), 0));
            self.tab_unread.remove(&id);
        }
        if self.closed_tabs.len() > tabs::CLOSED_LIMIT {
            let excess = self.closed_tabs.len() - tabs::CLOSED_LIMIT;
            self.closed_tabs.drain(0..excess);
        }
        if self.route == Route::Session && !self.tabs.contains(&self.sid()) {
            if let Some(id) = self.tabs.get(index.min(self.tabs.len().saturating_sub(1))).cloned() {
                self.open_session(&id);
            }
        }
    }

    fn sid(&self) -> String {
        self.handle.id()
    }

    // ---------------------------------------------------------------- models

    fn valid_model(&self, m: &str) -> bool {
        crate::models::resolve(&self.app.providers.read().unwrap(), m.split('#').next().unwrap_or(m)).is_ok()
    }

    /// Model for the current agent: explicit, session, agent, config, recent, default.
    fn pick_model(&mut self, explicit: Option<String>) {
        let session = self.handle.with(|s| s.model.clone());
        let agent = self.app.agent(&self.agent).ok().and_then(|a| a.model.clone());
        let candidates = [explicit, session, agent, self.app.config().model.clone()];
        let mut chosen = candidates.into_iter().flatten().find(|m| self.valid_model(m));
        if chosen.is_none() {
            chosen = self.prefs.recent.iter().find(|m| self.valid_model(m)).cloned();
        }
        if chosen.is_none() {
            chosen = self.app.default_model();
        }
        let (m, v) = match chosen {
            Some(r) => match r.split_once('#') {
                Some((m, v)) => (Some(m.to_string()), Some(v.to_string())),
                None => (Some(r), None),
            },
            None => (None, None),
        };
        self.model = m;
        self.variant = v.or_else(|| self.model.as_ref().and_then(|m| self.prefs.variant.get(m)).and_then(|v| v.as_str()).filter(|v| *v != "default").map(String::from));
        if let Some(model) = self.current_model() {
            if self.variant.as_ref().is_some_and(|v| !model.variants().contains(v)) {
                self.variant = None;
            }
        }
    }

    fn current_model(&self) -> Option<crate::models::Model> {
        let m = self.model.as_ref()?;
        crate::models::resolve(&self.app.providers.read().unwrap(), m).ok().map(|r| r.model)
    }

    /// (model label, provider label) as in the prompt metadata row.
    fn model_labels(&self) -> (String, String) {
        let Some(m) = &self.model else { return ("No provider selected".into(), "Connect a provider".into()) };
        let providers = self.app.providers.read().unwrap();
        let (pid, mid) = m.split_once('/').unwrap_or(("", m));
        let pname = providers.get(pid).map(|p| p.name.clone()).unwrap_or_else(|| pid.to_string());
        match crate::models::resolve(&providers, m) {
            Ok(r) => (r.model.name, pname),
            Err(_) => (format!("{mid} (unavailable)"), pname),
        }
    }

    /// Visible agents in upstream order; their index picks the categorical color.
    fn agent_order(&self) -> Vec<String> {
        let agents = self.app.agents();
        let mut ids: Vec<&crate::agent::Agent> = agents.values().filter(|a| !a.hidden).collect();
        ids.sort_by_key(|a| (a.id != "build", a.id != "plan", !a.is_primary(), a.id.clone()));
        ids.into_iter().map(|a| a.id.clone()).collect()
    }

    fn agent_color(&self, id: &str) -> Rgba {
        let colors = self.theme.agent_colors();
        let order = self.agent_order();
        let Some(i) = order.iter().position(|a| a == id) else { return colors[0] };
        if let Some(c) = self.app.agent(id).ok().and_then(|a| a.color.as_deref().and_then(Rgba::from_hex)) {
            return c;
        }
        colors[i % colors.len()]
    }

    fn primary_agents(&self) -> Vec<String> {
        let agents = self.app.agents();
        let mut v: Vec<&crate::agent::Agent> = agents.values().filter(|a| a.is_primary() && !a.hidden).collect();
        v.sort_by_key(|a| (a.id != "build", a.id != "plan", a.id.clone()));
        v.into_iter().map(|a| a.id.clone()).collect()
    }

    // ---------------------------------------------------------------- theme

    fn theme_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![crate::paths::get().config.clone(), self.app.worktree.join(".opencode"), self.cwd().join(".opencode")]
    }

    fn publish_theme(&self) {
        let mode = if self.mode == Mode::Light { "light" } else { "dark" };
        self.app.plugins.set_tui_theme(theme::plugin_theme(&self.theme), mode);
    }

    fn set_theme(&mut self, name: &str) -> bool {
        let sources = theme::sources(&self.theme_dirs());
        let found = sources.iter().find(|(n, _)| n == name).or_else(|| sources.iter().find(|(n, _)| n == "opencode"));
        if let Some((n, src)) = found {
            match theme::load(n, src, self.mode) {
                Ok(t) => {
                    self.theme = t;
                    self.cache = None;
                    self.publish_theme();
                    return n == name;
                }
                Err(e) => self.toast(Variant::Error, Some("Invalid theme".into()), format!("{name}: {e}")),
            }
        }
        false
    }

    // ---------------------------------------------------------------- toasts

    /// Upstream `toast.show`: a new toast replaces the visible one (it only queues while a toast is
    /// paused under the mouse, which this client doesn't do).
    fn toast(&mut self, variant: Variant, title: Option<String>, message: impl Into<String>) {
        self.toasts.clear();
        self.toasts.push_back(Toast { title, message: message.into(), variant, until: Instant::now() + TOAST_DURATION });
    }

    fn info(&mut self, m: impl Into<String>) {
        self.toast(Variant::Info, None, m);
    }

    // ---------------------------------------------------------------- commands

    fn commands(&self) -> Vec<Command> {
        let c = |id: &str, title: &str, category: &str, slash: &[&str], suggested: bool| Command {
            id: id.to_string(),
            title: title.to_string(),
            category: category.to_string(),
            slash: slash.iter().map(|value| value.to_string()).collect(),
            suggested,
            palette: true,
        };
        let hidden = |id: &str, title: &str, category: &str| Command {
            id: id.to_string(),
            title: title.to_string(),
            category: category.to_string(),
            slash: Vec::new(),
            suggested: false,
            palette: false,
        };
        let in_session = self.route == Route::Session;
        let sidebar = if self.sidebar_visible() { "Hide sidebar" } else { "Show sidebar" };
        let thinking = if self.settings.thinking_shown() { "Collapse thinking" } else { "Expand thinking" };
        let mut v = vec![
            c("session.list", "Switch session", "Session", &["sessions", "resume", "continue"], false),
            c("open.menu", "Open session or project", "Session", &["open", "projects", "project"], false),
            c("session.new", "New session", "Session", &["new"], in_session),
            c("session.clear", "Clear session", "Session", &["clear"], false),
            c("model.list", "Switch model", "Agent", &["models"], true),
            c("agent.list", "Switch agent", "Agent", &["agents"], false),
            c("mcp.list", "MCP servers", "Agent", &["mcps"], false),
            c("usage.show", "View usage", "Agent", &["usage"], false),
            c("provider.connect", "Connect an integration", "Integration", &["connect"], false),
            c("opencode.highlights", "Show release highlights", "System", &["highlights"], false),
            c("opencode.update", "Update OpenCode", "System", &["update"], false),
            c("server.pair", "Pair device", "System", &["pair", "web"], false),
            c("service.restart", "Restart service", "System", &["restart"], false),
            c("location.reload", "Reload configuration", "System", &["reload"], false),
            c("opencode.debug", "View debug info", "System", &["debug"], false),
            c("plugins.list", "Plugins", "System", &["plugins"], false),
            c("diff.open", "Open diff viewer", "VCS", &["diff"], false),
            c("stats.open", "Usage statistics", "System", &["stats"], false),
            c("variant.cycle", "Variant cycle", "Agent", &[], false),
            hidden("agent.cycle", "Agent cycle", "Agent"),
            hidden("agent.cycle.reverse", "Agent cycle reverse", "Agent"),
            hidden("model.cycle_recent", "Model cycle", "Agent"),
            hidden("model.cycle_recent_reverse", "Model cycle reverse", "Agent"),
            c("opencode.settings", "Open settings", "System", &["settings"], true),
            c("opencode.status", "View status", "System", &["status"], false),
            c("theme.switch", "Switch theme", "System", &["themes"], false),
            c("help.show", "Help", "System", &["help"], false),
            c("docs.open", "Open docs", "System", &[], false),
            c("app.exit", "Exit the app", "System", &["exit", "quit", "q"], false),
            c("session.sidebar.toggle", sidebar, "Session", &[], false),
            c("prompt.clear", "Clear prompt", "Prompt", &[], false),
            c("prompt.editor", "Open editor", "Session", &["editor"], false),
            hidden("session.interrupt", "Interrupt session", "Session"),
            hidden("session.toggle.thinking", thinking, "Session"),
            hidden("session.page.up", "Page up", "Session"),
            hidden("session.page.down", "Page down", "Session"),
            hidden("session.line.up", "Line up", "Session"),
            hidden("session.line.down", "Line down", "Session"),
            hidden("session.half.page.up", "Half page up", "Session"),
            hidden("session.half.page.down", "Half page down", "Session"),
            hidden("session.first", "First message", "Session"),
            hidden("session.last", "Last message", "Session"),
            hidden("session.messages_last_user", "Jump to last user message", "Session"),
        ];
        let mut variant = c("variant.list", "Switch model variant", "Agent", &["variants", "thinking", "effort"], false);
        variant.palette = self.current_model().is_some_and(|m| !m.variants().is_empty());
        v.push(variant);
        if in_session {
            v.extend([
                c("prompt.skills", "Skills", "Prompt", &["skills"], false),
                c("session.cd", "Change working directory", "Session", &["cd"], false),
                c("session.worktrees", "Manage workspaces", "Session", &["worktrees"], false),
                c("session.terminal", "New terminal", "Session", &["terminal"], false),
                c("terminal.toggle", "Toggle terminal pane", "Session", &[], false),
                c("terminal.select", "Select terminal", "Session", &[], false),
                c("terminal.close", "Close terminal pane", "Session", &[], false),
                c("session.aside", "Ask a side question", "Session", &["btw"], false),
                c("session.child.first", "Toggle subagent picker", "Session", &[], false),
                c("session.queued_prompts", "View queued prompts", "Prompt", &[], false),
                c("session.goal", "Session goal", "Session", &["goal"], false),
                c("session.message.user.next", "Next user message", "Session", &[], false),
                c("session.message.user.previous", "Previous user message", "Session", &[], false),
                c("session.child.next", "Next subagent", "Session", &[], false),
                c("session.child.previous", "Previous subagent", "Session", &[], false),
                c("session.parent", "Go to parent session", "Session", &[], false),
                c("prompt.stash", "Stash prompt", "Prompt", &[], false),
                c("prompt.stash.pop", "Pop stashed prompt", "Prompt", &[], false),
                c("prompt.stash.list", "List stashed prompts", "Prompt", &[], false),
                c("session.rename", "Rename session", "Session", &["rename"], false),
                c("session.timeline", "Jump to message", "Session", &["timeline"], false),
                c("session.fork", "Fork session", "Session", &["fork"], false),
                c("session.compact", "Compact session", "Session", &["compact"], false),
                c("session.undo", "Undo previous message", "Session", &["undo"], false),
                c("session.redo", "Redo", "Session", &["redo"], false),
                c("messages.copy", "Copy last assistant message", "Session", &["copy"], false),
                c("session.copy.id", "Copy session ID", "Session", &[], false),
                c("session.copy", "Copy session transcript", "Session", &[], false),
                c("session.export", "Export session transcript", "Session", &["export"], false),
                c("session.verbosity.cycle", if self.settings.grouping() { "Show tool calls individually" } else { "Group related tool calls" }, "Session", &[], false),
            ]);
        }
        for command in self.app.plugins.tui_commands() {
            let Some(name) = command.get("id").and_then(Value::as_str) else { continue };
            let title = command.get("title").and_then(Value::as_str).unwrap_or(name);
            let mut slash = Vec::new();
            if let Some(primary) = command.pointer("/slash/name").and_then(Value::as_str) {
                slash.push(primary.to_string());
                if let Some(aliases) = command.pointer("/slash/aliases").and_then(Value::as_array) {
                    slash.extend(aliases.iter().filter_map(Value::as_str).map(String::from));
                }
            }
            v.push(Command {
                id: format!("tui:{name}"),
                title: title.to_string(),
                category: command.get("group").and_then(Value::as_str).unwrap_or("Plugin").to_string(),
                slash,
                suggested: false,
                palette: true,
            });
        }
        for page in self.app.plugins.tui_pages() {
            let Some(id) = page.get("id").and_then(Value::as_str) else { continue };
            let name = page.get("name").and_then(Value::as_str).unwrap_or(id);
            v.push(Command {
                id: format!("tui.page:{id}"),
                title: name.to_string(),
                category: "Plugin".into(),
                slash: Vec::new(),
                suggested: false,
                palette: true,
            });
        }
        v
    }

    fn slash_commands(&self) -> Vec<(String, String, Pick)> {
        let mut out: Vec<(String, String, Pick)> = Vec::new();
        for c in self.commands() {
            for s in &c.slash {
                let pick = if c.id == "session.rename" { Pick::Slash(s.clone()) } else { Pick::Command(c.id.clone()) };
                out.push((format!("/{s}"), c.title.clone(), pick));
            }
        }
        let commands = self.app.commands();
        for c in commands.values() {
            out.push((format!("/{}", c.name), c.description.clone(), Pick::Slash(c.name.clone())));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }

    fn exec(&mut self, id: &str, args: &str) {
        match id {
            "command.palette.show" => self.open_palette(),
            "session.list" => self.open_sessions(false),
            "open.menu" => self.open_open(),
            "usage.show" => self.open_usage(),
            "provider.connect" => self.open_connect(),
            "opencode.highlights" => self.show_text("Release highlights", reports::highlights()),
            "opencode.update" => self.show_text("Update", reports::update()),
            "server.pair" => self.pair_device(),
            "service.restart" => self.restart_service(),
            "location.reload" => self.reload_config(),
            "opencode.debug" => self.show_text("Debug", reports::debug(&self.app)),
            "plugins.list" => self.show_text("Plugins", reports::plugins(&self.app)),
            "diff.open" => self.open_diff(),
            "stats.open" => self.open_stats(),
            "session.new" | "session.clear" => self.new_session(),
            "session.tab.next" => self.cycle_tab(1),
            "session.tab.previous" => self.cycle_tab(-1),
            "session.tab.close" => self.close_tab(),
            "session.tab.reopen" => self.reopen_tab(),
            id if id.starts_with("session.tab.select.") => {
                let n: usize = id["session.tab.select.".len()..].parse().unwrap_or(1);
                if let Some(t) = self.tabs.get(n.saturating_sub(1)).cloned() {
                    self.open_session(&t);
                }
            }
            "model.list" => self.open_models(),
            "agent.list" => self.open_agents(),
            "mcp.list" => self.open_mcp(),
            "variant.list" => self.open_variants(),
            "variant.cycle" => self.cycle_variant(),
            "agent.cycle" => self.cycle_agent(1),
            "agent.cycle.reverse" => self.cycle_agent(-1),
            "model.cycle_recent" => self.cycle_model(1),
            "model.cycle_recent_reverse" => self.cycle_model(-1),
            "opencode.settings" => self.open_settings(),
            "opencode.status" => self.dialog = Some(Dialog::Status),
            "theme.switch" => self.open_themes(),
            "help.show" => self.dialog = Some(Dialog::Help),
            "docs.open" => open_url("https://opencode.ai/docs"),
            "app.exit" => self.quit = true,
            "session.sidebar.toggle" => self.toggle_sidebar(),
            "prompt.clear" => self.clear_prompt(),
            "prompt.editor" => self.pending_editor = true,
            "session.interrupt" => self.interrupt_press(),
            "session.toggle.thinking" => {
                let next = if self.settings.thinking_shown() { "hide" } else { "show" };
                self.settings.set("session.thinking", json!(next));
                self.cache = None;
            }
            "session.verbosity.cycle" => {
                let next = if self.settings.grouping() { "none" } else { "auto" };
                self.settings.set("session.grouping", json!(next));
                self.cache = None;
            }
            "session.page.up" => self.scroll_by(self.body_height as isize / 2),
            "session.page.down" => self.scroll_by(-(self.body_height as isize / 2)),
            "session.half.page.up" => self.scroll_by(self.body_height as isize / 4),
            "session.half.page.down" => self.scroll_by(-(self.body_height as isize / 4)),
            "session.line.up" => self.scroll_by(1),
            "session.line.down" => self.scroll_by(-1),
            "session.first" => {
                self.scroll = usize::MAX / 2;
                if let Some(cache) = self.cache.as_mut() {
                    cache.older_loaded = false;
                }
            }
            "session.last" => self.scroll = 0,
            "session.messages_last_user" => self.jump_last_user(),
            "session.message.user.next" => self.jump_user(1),
            "session.message.user.previous" => self.jump_user(-1),
            "session.child.next" => self.child_step(1),
            "session.child.previous" => self.child_step(-1),
            "session.parent" => self.go_parent(),
            "session.rename" => {
                if args.trim().is_empty() {
                    let title = self.handle.with(|s| s.title.clone());
                    let mut e = Editor::default();
                    e.set(&title);
                    self.dialog = Some(Dialog::Rename(dialog::Prompt { title: "Rename Session".into(), editor: e, target: self.sid() }));
                } else {
                    let id = self.sid();
                    self.rename(&id, args.trim());
                }
            }
            "prompt.skills" => self.open_skills(),
            "session.cd" => self.open_cd(args),
            "session.worktrees" => self.open_worktrees(),
            "session.terminal" => self.open_terminal(),
            "terminal.toggle" => {
                if self.terminal_pane.visible {
                    self.terminal_pane.visible = false;
                    self.terminal_pane.focus = false;
                } else if self.terminal_pane.tabs.is_empty() {
                    self.open_terminal();
                } else {
                    self.terminal_pane.visible = true;
                    self.terminal_pane.focus = true;
                }
                self.cache = None;
            }
            "terminal.select" => self.open_composer(2),
            "terminal.close" => {
                self.terminal_pane.close_active();
                self.cache = None;
            }
            "session.aside" => self.open_btw(args),
            "session.child.first" => {
                if self.composer_open { self.close_composer() } else { self.open_composer(0) }
            }
            "session.queued_prompts" => self.open_queued(),
            "queued_prompt.undo" => {
                let message = self.handle.queue.lock().unwrap().pop_back().map(|(message, _)| message);
                if let Some(message) = message {
                    self.editor.set(&message.text());
                    self.update_auto();
                }
            }
            "queued_prompt.delete" => {
                self.handle.queue.lock().unwrap().pop_back();
            }
            "session.goal" => self.goal_command(args),
            "prompt.stash" => self.stash_prompt(),
            "prompt.stash.pop" => self.pop_stash(),
            "prompt.stash.list" => self.open_stash(),
            "session.timeline" => self.open_timeline(Kind::Timeline),
            "session.fork" => self.open_timeline(Kind::Fork),
            "session.compact" => self.compact(),
            "session.undo" => self.undo(),
            "session.redo" => self.redo(),
            "messages.copy" => {
                let text = self.handle.with(|s| s.last_assistant().map(|m| m.parts.iter().filter_map(|p| if let Part::Text { text, .. } = p { Some(text.clone()) } else { None }).collect::<Vec<_>>().join("\n")));
                match text.map(|t| t.trim().to_string()) {
                    None => self.toast(Variant::Error, None, "No assistant messages found"),
                    Some(t) if t.is_empty() => self.toast(Variant::Error, None, "No text content found in last assistant message"),
                    Some(t) => {
                        copy_to_clipboard(&t);
                        self.toast(Variant::Success, None, "Message copied to clipboard!");
                    }
                }
            }
            "session.copy.id" => {
                copy_to_clipboard(&self.sid());
                self.toast(Variant::Success, None, "Session ID copied to clipboard!");
            }
            "session.copy" => {
                copy_to_clipboard(&self.transcript_markdown(true, true));
                self.toast(Variant::Success, None, "Session transcript copied to clipboard!");
            }
            "session.export" => self.export(),
            id if id.starts_with("tui.page:") => {
                let page = id["tui.page:".len()..].to_string();
                let app = self.app.clone();
                tokio::spawn(async move {
                    match app.plugins.execute_tui_page(&page, Value::Null).await {
                        Ok(value) => {
                            let title = value.get("title").and_then(Value::as_str).unwrap_or(&page).to_string();
                            let text = value.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                            let layout = value.get("layout").cloned().unwrap_or(Value::Null);
                            app.emit(AppEvent::PluginPage { page: page.clone(), title, text, layout });
                        }
                        Err(error) => app.emit(AppEvent::Notice { message: format!("Plugin page failed: {error}") }),
                    }
                });
            }
            id if id.starts_with("tui:") => {
                let name = id["tui:".len()..].to_string();
                let input = args.trim().to_string();
                let app = self.app.clone();
                tokio::spawn(async move {
                    if let Err(error) = app.plugins.execute_tui_command(&name, json!(input)).await {
                        app.emit(AppEvent::Notice { message: format!("Plugin command failed: {error}") });
                    }
                });
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- actions

    fn new_session(&mut self) {
        self.handle = self.app.new_session(None);
        self.route = Route::Home;
        self.cache = None;
        self.scroll = 0;
        self.redo.clear();
        self.dialog = None;
    }

    fn open_session(&mut self, value: &str) {
        let loaded = match value.split_once('|') {
            Some((project, id)) => self.app.session_in(project, id),
            None => self.app.session(value),
        };
        match loaded {
            Ok(h) => {
                let id = h.id();
                if id != self.sid() {
                    self.terminal_pane.clear();
                }
                self.handle = h;
                self.route = Route::Session;
                self.cache = None;
                self.scroll = 0;
                self.tab_unread.remove(&id);
                self.redo.clear();
                self.visit(&id);
                if let Some(a) = self.handle.with(|s| s.agent.clone()).filter(|a| self.app.agent(a).is_ok()) {
                    self.agent = a;
                }
                self.pick_model(None);
            }
            Err(_) => self.toast(Variant::Error, None, format!("Session not found: {value}")),
        }
    }

    fn rename(&mut self, id: &str, title: &str) {
        if let Ok(h) = self.app.session(id) {
            h.with(|s| s.title = title.to_string());
            h.save();
            self.cache = None;
        }
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

    fn undo(&mut self) {
        let idx = self.handle.with(|s| s.messages.iter().rposition(|m| m.role == Role::User && m.parts.iter().any(|p| matches!(p, Part::Text { synthetic: false, text } if !text.trim().is_empty()))));
        let Some(idx) = idx else {
            self.toast(Variant::Error, None, "Nothing to undo");
            return;
        };
        if self.handle.is_busy() {
            self.handle.abort();
            self.app.reject_all_pending(&self.sid());
        }
        let removed: Vec<Message> = self.handle.with(|s| s.messages.drain(idx..).collect());
        self.handle.save();
        let text = removed.first().map(|m| m.parts.iter().filter_map(|p| if let Part::Text { text, synthetic: false } = p { Some(text.clone()) } else { None }).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        self.redo.push((self.sid(), removed));
        self.editor.set(&text);
        self.cache = None;
    }

    fn redo(&mut self) {
        if let Some((sid, msgs)) = self.redo.pop() {
            if sid == self.sid() {
                self.handle.with(|s| s.messages.extend(msgs));
                self.handle.save();
                self.editor.clear();
                self.cache = None;
            }
        }
    }

    fn transcript_markdown(&self, thinking: bool, tools: bool) -> String {
        let s = self.handle.session.lock().unwrap().clone();
        let mut body = Vec::new();
        for m in &s.messages {
            match m.role {
                Role::User => {
                    let t = m.parts.iter().filter_map(|p| if let Part::Text { text, synthetic: false } = p { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
                    if !t.is_empty() {
                        body.push(format!("## User\n\n{t}"));
                    }
                }
                Role::Assistant => {
                    let mut content = Vec::new();
                    for p in &m.parts {
                        match p {
                            Part::Text { text, .. } => content.push(text.clone()),
                            Part::Reasoning { text, .. } if thinking && !text.trim().is_empty() => content.push(format!("_Thinking:_\n\n{text}")),
                            Part::Tool { name, input, state, .. } if tools => {
                                let out = match state {
                                    crate::session::ToolState::Completed { output, .. } => output.clone(),
                                    crate::session::ToolState::Error { error } => error.clone(),
                                    _ => String::new(),
                                };
                                content.push(format!("**Tool: {name}**\n\n**Input:**\n```json\n{}\n```\n\n{out}", serde_json::to_string_pretty(input).unwrap_or_default()));
                            }
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        body.push(format!("## Assistant\n\n{}", content.join("\n\n")));
                    }
                }
            }
        }
        let date = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%-m/%-d/%Y, %-I:%M:%S %p").to_string()).unwrap_or_default();
        format!("# {}\n\n**Session ID:** {}\n**Created:** {}\n**Updated:** {}\n\n---\n\n{}\n", s.title, s.id, date(s.time_created), date(s.time_updated), body.join("\n\n---\n\n"))
    }

    fn export(&mut self) {
        let md = self.transcript_markdown(false, true);
        let directory = self.cwd();
        let path = directory.join(format!("session-{}.md", crate::util::id("exp").trim_start_matches("exp_")));
        match std::fs::write(&path, md) {
            Ok(_) => self.toast(Variant::Success, Some("Exported".into()), path.display().to_string()),
            Err(_) => self.toast(Variant::Error, None, "Failed to export session"),
        }
    }

    fn jump_last_user(&mut self) {
        if let Some(c) = &self.cache {
            if let Some(&start) = c.out.users.last() {
                self.scroll = c.out.lines.len().saturating_sub(start + self.body_height);
            }
        }
    }

    fn jump_user(&mut self, step: isize) {
        let Some(cache) = &self.cache else { return };
        let total = cache.out.lines.len();
        let h = self.body_height;
        let top = total.saturating_sub(h + self.scroll);
        let target = if step > 0 {
            cache.out.users.iter().copied().find(|line| *line > top + 1)
        } else {
            cache.out.users.iter().rev().copied().find(|line| *line + 1 < top)
        };
        if let Some(line) = target {
            self.scroll = total.saturating_sub(line + h);
        }
    }

    fn child_sessions(&self) -> Vec<crate::session::Summary> {
        let sid = self.sid();
        crate::session::list(&self.app.project)
            .into_iter()
            .filter(|session| session.parent_id.as_deref() == Some(sid.as_str()))
            .collect()
    }

    fn child_step(&mut self, step: isize) {
        let mut children = self.child_sessions();
        if children.is_empty() {
            return;
        }
        children.sort_by_key(|session| session.time_created);
        let current = self.sid();
        let index = children.iter().position(|session| session.id == current).unwrap_or(0) as isize + step;
        let index = index.rem_euclid(children.len() as isize) as usize;
        let id = children[index].id.clone();
        self.open_session(&id);
    }

    fn go_parent(&mut self) {
        let parent = self.handle.with(|session| session.parent_id.clone());
        if let Some(parent) = parent {
            self.open_session(&parent);
        }
    }

    fn cycle_variant(&mut self) {
        let Some(model) = self.current_model() else { return };
        let vs = model.variants();
        if vs.is_empty() {
            return;
        }
        self.variant = match &self.variant {
            None => Some(vs[0].clone()),
            Some(v) => vs.iter().position(|x| x == v).and_then(|i| vs.get(i + 1)).cloned(),
        };
        self.remember_variant();
    }

    fn remember_variant(&mut self) {
        if let Some(m) = self.model.clone() {
            self.prefs.variant.insert(m, json!(self.variant.clone().unwrap_or_else(|| "default".into())));
            self.prefs.save();
        }
    }

    fn cycle_agent(&mut self, dir: isize) {
        let agents = self.primary_agents();
        if agents.is_empty() {
            return;
        }
        let i = agents.iter().position(|a| *a == self.agent).unwrap_or(0) as isize;
        let n = agents.len() as isize;
        self.agent = agents[(i + dir).rem_euclid(n) as usize].clone();
        if let Some(m) = self.app.agent(&self.agent).ok().and_then(|a| a.model.clone()) {
            self.pick_model(Some(m));
        }
    }

    fn cycle_model(&mut self, dir: isize) {
        let recent: Vec<String> = self.prefs.recent.iter().filter(|m| self.valid_model(m)).cloned().collect();
        if recent.is_empty() {
            return;
        }
        let cur = self.model.clone().unwrap_or_default();
        let next = match recent.iter().position(|m| *m == cur) {
            Some(i) => (i as isize + dir).rem_euclid(recent.len() as isize) as usize,
            None if dir > 0 => 0,
            None => recent.len() - 1,
        };
        let m = recent[next].clone();
        self.set_model(&m, false);
    }

    fn set_model(&mut self, m: &str, remember: bool) {
        self.model = Some(m.to_string());
        self.variant = self.prefs.variant.get(m).and_then(|v| v.as_str()).filter(|v| *v != "default").map(String::from);
        if let Some(model) = self.current_model() {
            if self.variant.as_ref().is_some_and(|v| !model.variants().contains(v)) {
                self.variant = None;
            }
        }
        if remember {
            self.prefs.use_model(m);
        }
    }

    fn sidebar_visible(&self) -> bool {
        if self.mini || self.route != Route::Session || self.handle.with(|s| s.parent_id.is_some()) {
            return false;
        }
        match self.sidebar {
            Some(v) => v,
            None => self.settings.sidebar_auto() && self.size.0 > 120,
        }
    }

    fn toggle_sidebar(&mut self) {
        let visible = self.sidebar_visible();
        self.settings.set("session.sidebar", json!(if visible { "hide" } else { "auto" }));
        self.sidebar = Some(!visible);
        self.cache = None;
    }

    fn clear_prompt(&mut self) {
        self.editor.clear();
        self.pasted.clear();
        self.auto = None;
        self.history.reset();
    }

    fn interrupt_press(&mut self) {
        if !self.handle.is_busy() {
            return;
        }
        match self.interrupt {
            Some(t) if t.elapsed() < Duration::from_secs(5) => {
                self.handle.abort();
                self.app.reject_all_pending(&self.sid());
                self.interrupt = None;
            }
            _ => self.interrupt = Some(Instant::now()),
        }
    }

    // ---------------------------------------------------------------- submit

    fn expand_pasted(&self, text: &str) -> String {
        let mut t = text.to_string();
        for (placeholder, full) in &self.pasted {
            t = t.replacen(placeholder.as_str(), full, 1);
        }
        t
    }

    fn submit(&mut self) {
        let raw = self.editor.text.clone();
        if raw.trim().is_empty() {
            return;
        }
        let text = self.expand_pasted(raw.trim_end());
        self.history.append(&raw, &self.pasted, self.shell);
        if self.shell {
            self.clear_prompt();
            self.shell = false;
            self.run_shell(text);
            return;
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if let Some(cmd) = self.commands().into_iter().find(|c| c.slash.iter().any(|slash| slash == name)) {
                self.clear_prompt();
                self.exec(&cmd.id, args);
                return;
            }
            if let Some(cmd) = self.app.commands().get(name).cloned() {
                self.clear_prompt();
                if self.app.plugin_commands().contains(name) {
                    self.enter_session();
                    let app = self.app.clone();
                    let sid = self.sid();
                    let name = name.to_string();
                    let args = args.trim().to_string();
                    tokio::spawn(async move {
                        let _ = app
                            .plugins
                            .execute_command(&name, &sid, serde_json::json!({ "text": args }), "steer")
                            .await;
                    });
                    return;
                }
                if cmd.mcp_prompt.is_some() {
                    self.clear_prompt();
                    self.enter_session();
                    self.scroll = 0;
                    let app = self.app.clone();
                    let sid = self.sid();
                    let args = args.trim().to_string();
                    let agent = self.agent.clone();
                    let model = self.model.clone();
                    let variant = self.variant.clone();
                    tokio::spawn(async move {
                        let text = crate::command::render_mcp(&app, &cmd, &args).await;
                        let mut msg = Message::user_text(text.clone());
                        msg.parts.extend(crate::mentions::expand_mentions(&app, &text));
                        msg.skills = crate::mentions::skill_mentions(&app, &text);
                        let opts = PromptOptions {
                            agent: cmd.agent.clone().or(Some(agent)),
                            model: cmd.model.clone().or(model),
                            variant,
                            delivery: None,
                        };
                        let _ = crate::engine::prompt(app, &sid, msg, opts).await;
                    });
                    return;
                }
                let directory = self.cwd();
                let rendered = crate::command::render(&cmd, args.trim(), &directory, &self.app.worktree);
                self.send(rendered, Some(&cmd));
                return;
            }
        }
        self.clear_prompt();
        self.send(text, None);
    }

    fn enter_session(&mut self) {
        if self.route == Route::Home {
            self.route = Route::Session;
            let id = self.sid();
            self.visit(&id);
        }
    }

    fn send(&mut self, text: String, cmd: Option<&crate::command::Command>) {
        if self.model.is_none() && cmd.and_then(|c| c.model.clone()).is_none() {
            self.toast(Variant::Warning, None, "Connect an integration to send prompts");
            return;
        }
        let mut msg = Message::user_text(text.clone());
        msg.parts.extend(crate::mentions::expand_mentions(&self.app, &text));
        msg.skills = crate::mentions::skill_mentions(&self.app, &text);
        let opts = PromptOptions {
            agent: cmd.and_then(|c| c.agent.clone()).or_else(|| Some(self.agent.clone())),
            model: cmd.and_then(|c| c.model.clone()).or_else(|| self.model.clone()),
            variant: self.variant.clone(),
            delivery: None,
        };
        if let Some(m) = self.model.clone() {
            self.prefs.use_model(&m);
        }
        self.enter_session();
        self.scroll = 0;
        let app = self.app.clone();
        let sid = self.sid();
        tokio::spawn(async move {
            let _ = crate::engine::prompt(app, &sid, msg, opts).await;
        });
    }

    /// Shell mode: run the command locally and record it in the session for the model.
    fn run_shell(&mut self, command: String) {
        self.enter_session();
        let app = self.app.clone();
        let handle = self.handle.clone();
        let sid = self.sid();
        self.shells.push(command.clone());
        tokio::spawn(async move {
            let shell = crate::tools::shell::detect();
            let directory = handle.with(|session| session.directory.clone());
            let out = tokio::process::Command::new(&shell.program).args(&shell.args).arg(&command).current_dir(&directory).output().await;
            let (text, code) = match out {
                Ok(o) => (format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)), o.status.code().unwrap_or(-1)),
                Err(e) => (e.to_string(), -1),
            };
            let mut m = Message::new(Role::User);
            m.parts.push(Part::Text { text: format!("<shell-run command=\"{}\" exit=\"{code}\">\n{}\n</shell-run>", command.replace('"', "&quot;"), text.trim_end()), synthetic: true });
            handle.with(|s| s.messages.push(m));
            handle.save();
            app.emit(AppEvent::Updated { session: sid });
        });
    }

    // ---------------------------------------------------------------- dialogs

    fn shortcut(&self, id: &str) -> String {
        self.keys.labels(id).unwrap_or_default()
    }

    fn open_palette(&mut self) {
        let cmds: Vec<Command> = self.commands().into_iter().filter(|c| c.palette).collect();
        let mut opts: Vec<Opt> = cmds
            .iter()
            .filter(|c| c.suggested)
            .map(|c| Opt { title: c.title.clone(), footer: self.shortcut(&c.id), category: "Suggested".into(), value: c.id.clone(), search: c.id.clone(), ..Default::default() })
            .collect();
        let pinned = opts.len();
        for c in &cmds {
            opts.push(Opt { title: c.title.clone(), footer: self.shortcut(&c.id), category: c.category.clone(), value: c.id.clone(), search: c.id.clone(), ..Default::default() });
        }
        let commands = self.app.commands();
        for c in commands.values() {
            opts.push(Opt { title: format!("/{}", c.name), description: c.description.clone(), category: "Commands".into(), value: format!("slash:{}", c.name), ..Default::default() });
        }
        let mut s = Select::new(Kind::Palette, "Commands", opts);
        s.flat = true;
        s.pinned = pinned;
        self.dialog = Some(Dialog::Select(s));
    }

    fn open_models(&mut self) {
        let providers = self.app.providers();
        let available = crate::models::available(&self.app.config(), &providers);
        let current = self.model.clone().unwrap_or_default();
        let mut opts = Vec::new();
        let mut shown: HashSet<String> = HashSet::new();
        let free = |m: &crate::models::Model| m.cost_input == 0.0 && m.cost_output == 0.0;
        let pname = |id: &str| providers.get(id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
        for (cat, list) in [("Favorites", &self.prefs.favorite), ("Recent", &self.prefs.recent)] {
            for r in list.iter() {
                if shown.contains(r) {
                    continue;
                }
                if let Ok(res) = crate::models::resolve(&providers, r) {
                    if !available.contains(&res.model.provider_id) {
                        continue;
                    }
                    shown.insert(r.clone());
                    opts.push(Opt {
                        title: res.model.name.clone(),
                        description: pname(&res.model.provider_id),
                        footer: if free(&res.model) { "Free".into() } else { String::new() },
                        category: cat.into(),
                        value: r.clone(),
                        current: *r == current,
                        ..Default::default()
                    });
                }
            }
        }
        // upstream `sortModelOptions`: OpenCode Go, then OpenCode, then by provider name; free models first
        let mut order: Vec<&String> = available.iter().collect();
        order.sort_by_key(|id| (*id != "opencode-go", *id != "opencode", pname(id)));
        for id in order {
            let p = &providers[id];
            let mut ms: Vec<_> = p.models.values().filter(|m| m.status.as_deref() != Some("deprecated")).collect();
            ms.sort_by(|a, b| free(b).cmp(&free(a)).then(b.release_date.cmp(&a.release_date)).then(a.name.cmp(&b.name)));
            for m in ms {
                let full = m.full_id();
                if shown.contains(&full) {
                    continue;
                }
                let fav = self.prefs.favorite.contains(&full);
                opts.push(Opt {
                    title: m.name.clone(),
                    description: if fav { "(Favorite)".into() } else { String::new() },
                    footer: if free(m) { "Free".into() } else { String::new() },
                    category: p.name.clone(),
                    current: full == current,
                    search: p.name.clone(),
                    value: full,
                    ..Default::default()
                });
            }
        }
        if opts.is_empty() {
            self.toast(Variant::Warning, None, "Connect an integration to send prompts");
            return;
        }
        let mut s = Select::new(Kind::Models, "Select model", opts);
        s.flat = true;
        s.hints = vec![
            Hint { title: "Connect an integration".into(), key: self.shortcut("model.dialog.provider"), right: false },
            Hint { title: "Favorite".into(), key: self.shortcut("model.dialog.favorite"), right: false },
        ];
        self.dialog = Some(Dialog::Select(s));
    }

    fn session_options(&self, all: bool) -> Vec<Opt> {
        let today = chrono::Local::now().date_naive();
        let mut list: Vec<crate::session::Summary> = if all {
            let root = crate::paths::get().storage().join("session");
            let mut v = Vec::new();
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    if let Some(p) = e.file_name().to_str() {
                        v.extend(crate::session::list(p));
                    }
                }
            }
            v.sort_by(|a, b| b.time_updated.cmp(&a.time_updated));
            v
        } else {
            crate::session::list(&self.app.project)
        };
        list.retain(|s| s.parent_id.is_none());
        let cur = self.sid();
        list.into_iter()
            .map(|s| {
                let d = chrono::DateTime::from_timestamp_millis(s.time_updated).map(|d| d.with_timezone(&chrono::Local).date_naive());
                let cat = match d {
                    Some(d) if d == today => "Today".to_string(),
                    Some(d) => d.format("%a %b %d %Y").to_string(),
                    None => String::new(),
                };
                let footer = if all && s.directory != self.app.directory { draw::truncate(&s.directory.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(), 25) } else { String::new() };
                Opt { title: s.title.clone(), footer, category: cat, current: s.id == cur, value: s.id, ..Default::default() }
            })
            .collect()
    }

    fn open_sessions(&mut self, all: bool) {
        let opts = self.session_options(all);
        let mut s = Select::new(Kind::Sessions, "Sessions", opts);
        s.size = Size::Large;
        s.all = all;
        if !all {
            s.suffix = format!(" for {}", self.app.worktree.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
        }
        s.hints = vec![
            Hint { title: "delete".into(), key: self.shortcut("session.delete"), right: false },
            Hint { title: "rename".into(), key: self.shortcut("session.rename"), right: false },
            Hint { title: if all { "current directory".into() } else { "all projects".into() }, key: "ctrl+a".into(), right: true },
        ];
        self.dialog = Some(Dialog::Select(s));
    }

    fn open_agents(&mut self) {
        let agents = self.app.agents();
        let opts = self
            .primary_agents()
            .into_iter()
            .map(|id| {
                let a = &agents[&id];
                Opt { title: id.clone(), description: a.description.clone().unwrap_or_default(), current: id == self.agent, value: id, ..Default::default() }
            })
            .collect();
        self.dialog = Some(Dialog::Select(Select::new(Kind::Agents, "Select agent", opts)));
    }

    fn open_skills(&mut self) {
        let mut skills = self.app.skills();
        skills.sort_by(|a, b| a.id.cmp(&b.id));
        if skills.is_empty() {
            self.toast(Variant::Warning, None, "No skills available");
            return;
        }
        let width = skills.iter().map(|skill| skill.name.chars().count()).max().unwrap_or(0);
        let opts: Vec<Opt> = skills
            .into_iter()
            .map(|skill| Opt {
                title: format!("{:width$}", skill.name, width = width),
                description: skill.description,
                current: self.editor.text.split_whitespace().any(|word| word == format!("@{}", skill.id)),
                value: skill.id,
                ..Default::default()
            })
            .collect();
        let mut s = Select::new(Kind::Skills, "Skills", opts);
        s.placeholder = "Search skills".into();
        s.size = Size::Large;
        self.dialog = Some(Dialog::Select(s));
    }

    fn open_variants(&mut self) {
        let Some(m) = self.current_model() else { return };
        let vs = m.variants();
        if vs.is_empty() {
            self.toast(Variant::Info, Some("No variants available".into()), "The current model does not support any variants.");
            return;
        }
        let cur = self.variant.clone().unwrap_or_else(|| "default".into());
        let mut opts = vec![Opt { title: "Default".into(), value: "default".into(), current: cur == "default", ..Default::default() }];
        for v in vs.into_iter().filter(|v| v != "default") {
            opts.push(Opt { title: v.clone(), current: cur == v, value: v, ..Default::default() });
        }
        let mut s = Select::new(Kind::Variants, "Select variant", opts);
        s.flat = true;
        self.dialog = Some(Dialog::Select(s));
    }

    fn open_themes(&mut self) {
        let mut names: Vec<String> = theme::sources(&self.theme_dirs()).into_iter().map(|(n, _)| n).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        let cur = self.theme.name.clone();
        let opts = names.into_iter().map(|n| Opt { title: n.clone(), current: n == cur, value: n, ..Default::default() }).collect();
        self.theme_before = Some(cur);
        self.dialog = Some(Dialog::Select(Select::new(Kind::Themes, "Themes", opts)));
    }

    fn open_mcp(&mut self) {
        let opts: Vec<Opt> = self
            .app
            .mcp
            .snapshot()
            .into_iter()
            .map(|(n, st)| {
                let status = match st {
                    Some(crate::mcp::Status::Connected { .. }) => "Connected",
                    Some(crate::mcp::Status::Disabled) => "Disabled",
                    Some(crate::mcp::Status::Failed(_)) => "Error",
                    Some(crate::mcp::Status::NeedsAuth(_)) => "Sign in required",
                    None => "Connecting",
                };
                Opt { title: n.clone(), footer: status.into(), value: n, ..Default::default() }
            })
            .collect();
        let mut select = Select::new(Kind::Mcp, "MCP servers", opts);
        select.hints.push(Hint { title: "sign in".into(), key: "enter".into(), right: false });
        self.dialog = Some(Dialog::Select(select));
    }

    /// Start the OAuth flow for a remote MCP server. Discovery and registration run off the UI
    /// thread; the dialog shows the authorization URL and the user opens it with enter.
    fn start_mcp_auth(&mut self, name: String) {
        let Some(config) = self.app.config().mcp.get(&name).cloned() else {
            self.toast(Variant::Error, None, format!("MCP server not configured: {name}"));
            return;
        };
        let app = self.app.clone();
        self.dialog = Some(Dialog::Text {
            title: name.clone(),
            text: "Starting authorization...".into(),
            scroll: 0,
            layout: Value::Null,
        });
        tokio::spawn(async move {
            let sign_in = match crate::mcp_oauth::begin(&name, &config).await {
                Ok(sign_in) => sign_in,
                Err(error) => {
                    app.emit(AppEvent::McpAuthDone { name, error: Some(error.to_string()) });
                    return;
                }
            };
            let url = sign_in.url.clone();
            let handle = sign_in.handle();
            let instructions = format!("Authorize {name} in your browser. This window will close automatically.");
            app.emit(AppEvent::McpAuth { name: name.clone(), url, instructions, handle: Some(std::sync::Arc::new(handle)) });
            if let Err(error) = crate::mcp_oauth::finish(sign_in).await {
                app.emit(AppEvent::McpAuthDone { name, error: Some(error.to_string()) });
                return;
            }
            let error = match app.mcp.connect_server(&name).await {
                Ok(crate::mcp::Status::Connected { .. }) => None,
                Ok(crate::mcp::Status::NeedsAuth(error)) => Some(format!("still needs authentication: {error}")),
                Ok(crate::mcp::Status::Failed(error)) => Some(error),
                Ok(crate::mcp::Status::Disabled) => Some("server is disabled".into()),
                Err(error) => Some(error.to_string()),
            };
            app.emit(AppEvent::McpAuthDone { name, error });
        });
    }

    fn retry_mcp(&mut self, name: String) {
        let app = self.app.clone();
        self.toast(Variant::Info, None, format!("Reconnecting {name}..."));
        tokio::spawn(async move {
            let error = match app.mcp.connect_server(&name).await {
                Ok(crate::mcp::Status::Connected { .. }) => None,
                Ok(crate::mcp::Status::NeedsAuth(error)) => Some(format!("sign in required: {error}")),
                Ok(crate::mcp::Status::Failed(error)) => Some(error),
                Ok(crate::mcp::Status::Disabled) => Some("server is disabled".into()),
                Err(error) => Some(error.to_string()),
            };
            app.emit(AppEvent::McpAuthDone { name, error });
        });
    }

    fn open_timeline(&mut self, kind: Kind) {
        let opts: Vec<Opt> = self.handle.with(|s| {
            s.messages
                .iter()
                .filter(|m| m.role == Role::User && m.parts.iter().any(|p| matches!(p, Part::Text { synthetic: false, .. })))
                .rev()
                .map(|m| {
                    let t = m.text().replace('\n', " ");
                    let time = chrono::DateTime::from_timestamp_millis(m.time).map(|d| d.with_timezone(&chrono::Local).format("%-I:%M %p").to_string()).unwrap_or_default();
                    Opt { title: t, footer: time, value: m.id.clone(), ..Default::default() }
                })
                .collect()
        });
        if opts.is_empty() {
            self.info("No messages yet");
            return;
        }
        let title = if kind == Kind::Fork { "Fork from message" } else { "Timeline" };
        self.dialog = Some(Dialog::Select(Select::new(kind, title, opts)));
    }

    fn open_settings(&mut self) {
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        let opts = vec![
            Opt { title: "Theme".into(), footer: self.theme.name.clone(), category: "Appearance".into(), value: "theme".into(), ..Default::default() },
            Opt { title: "Animations".into(), footer: on(self.settings.animations()), category: "Appearance".into(), value: "animations".into(), ..Default::default() },
            Opt { title: "Show thinking".into(), footer: on(self.settings.thinking_shown()), category: "Session".into(), value: "thinking".into(), ..Default::default() },
            Opt { title: "Sidebar".into(), footer: if self.settings.sidebar_auto() { "auto".into() } else { "hide".into() }, category: "Session".into(), value: "sidebar".into(), ..Default::default() },
            Opt { title: "Group related tool calls".into(), footer: on(self.settings.grouping()), category: "Session".into(), value: "grouping".into(), ..Default::default() },
            Opt { title: "Tokens per second".into(), footer: on(self.settings.tps()), category: "Session".into(), value: "tps".into(), ..Default::default() },
            Opt { title: "Tab mode".into(), footer: self.settings.tabs_mode().into(), category: "Tabs".into(), value: "tabs.mode".into(), ..Default::default() },
            Opt { title: "Tab scope".into(), footer: self.settings.tabs_scope().into(), category: "Tabs".into(), value: "tabs.scope".into(), ..Default::default() },
            Opt { title: "Tab layout".into(), footer: self.settings.tabs_layout().into(), category: "Tabs".into(), value: "tabs.layout".into(), ..Default::default() },
            Opt { title: "Tab indicators".into(), footer: self.settings.tabs_indicators().into(), category: "Tabs".into(), value: "tabs.indicators".into(), ..Default::default() },
        ];
        self.dialog = Some(Dialog::Select(Select::new(Kind::Settings, "Settings", opts)));
    }

    fn show_text(&mut self, title: &str, text: String) {
        self.dialog = Some(Dialog::Text { title: title.into(), text, scroll: 0, layout: Value::Null });
    }

    fn open_open(&mut self) {
        let current = (self.route == Route::Session).then(|| self.sid());
        let opts = navigation::open_options(&self.app, current.as_deref());
        if opts.is_empty() {
            self.info("No sessions found");
            return;
        }
        let mut s = Select::new(Kind::Open, "Open session or project", opts);
        s.size = Size::Large;
        self.dialog = Some(Dialog::Select(s));
    }

    fn open_usage(&mut self) {
        let local = reports::usage(&self.app, &self.handle);
        self.show_text("Usage", local.clone());
        let app = self.app.clone();
        tokio::spawn(async move {
            let remote = reports::usage_limits().await;
            app.emit(AppEvent::PluginPage {
                page: String::new(),
                title: "Usage".into(),
                text: format!("{local}\n\n{remote}"),
                layout: Value::Null,
            });
        });
    }

    fn open_diff(&mut self) {
        self.open_diff_source(None);
    }

    fn open_diff_source(&mut self, source: Option<&str>) {
        let source = source
            .map(String::from)
            .or_else(|| self.settings.str("diffs.source").map(String::from))
            .unwrap_or_else(|| "branch".into());
        let source = if source == "working" { diff_view::Source::Working } else { diff_view::Source::Branch };
        let view = self.settings.str("diffs.view").unwrap_or("auto");
        let split = view == "split" || (view == "auto" && self.size.0 >= 120);
        let tree = self.settings.bool("diffs.tree").unwrap_or(true);
        let single = self.settings.bool("diffs.single").unwrap_or(false);
        let fullscreen = self.settings.bool("diffs.fullscreen").unwrap_or(false);
        let wrap = self.settings.str("diffs.wrap") == Some("word");
        self.dialog = Some(Dialog::Diff(diff_view::DiffView::new(source, tree, single, split, fullscreen, wrap)));
        let app = self.app.clone();
        let directory = self.handle.with(|session| session.directory.clone());
        let name = source.name().to_string();
        tokio::spawn(async move {
            let mode = if name == "working" { "working" } else { "branch" };
            let dir = directory.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::vcs::diff(&serde_json::json!({
                    "directory": dir.to_string_lossy().to_string(),
                    "mode": mode,
                }))
            })
            .await;
            match result {
                Ok(Ok(files)) => app.emit(AppEvent::DiffLoaded { source: name, files }),
                Ok(Err(error)) => {
                    app.emit(AppEvent::DiffLoaded { source: name, files: Value::Null });
                    app.emit(AppEvent::Notice { message: format!("Diff unavailable: {error}") });
                }
                Err(error) => app.emit(AppEvent::Notice { message: format!("Diff failed: {error}") }),
            }
        });
    }

    fn open_stats(&mut self) {
        self.show_text("Usage statistics", "Reading session statistics...".into());
        let app = self.app.clone();
        tokio::spawn(async move {
            let text = tokio::task::spawn_blocking(reports::stats).await.unwrap_or_else(|error| format!("Statistics failed: {error}"));
            app.emit(AppEvent::PluginPage { page: String::new(), title: "Usage statistics".into(), text, layout: Value::Null });
        });
    }

    fn open_connect(&mut self) {
        let opts = actions::provider_choices(&self.app);
        if opts.is_empty() {
            self.info("No providers are available");
            return;
        }
        self.dialog = Some(Dialog::Select(Select::new(Kind::Connect, "Connect an integration", opts)));
    }

    fn open_cd(&mut self, args: &str) {
        if args.trim().is_empty() {
            let mut editor = Editor::default();
            editor.set(&self.handle.with(|s| s.directory.to_string_lossy().to_string()));
            self.dialog = Some(Dialog::Rename(dialog::Prompt {
                title: "Change working directory".into(),
                editor,
                target: "cd".into(),
            }));
            return;
        }
        self.change_directory(args.trim());
    }

    fn open_btw(&mut self, args: &str) {
        if args.trim().is_empty() {
            self.dialog = Some(Dialog::Rename(dialog::Prompt {
                title: "Ask a side question".into(),
                editor: Editor::default(),
                target: "btw".into(),
            }));
            return;
        }
        self.ask_btw(args.trim().to_string());
    }

    fn open_worktrees(&mut self) {
        let mut opts = navigation::workspace_options();
        opts.push(Opt {
            title: "Create workspace in current directory".into(),
            category: "Actions".into(),
            value: "workspace-new".into(),
            ..Default::default()
        });
        let mut s = Select::new(Kind::Workspaces, "Workspaces", opts);
        s.size = Size::Large;
        self.dialog = Some(Dialog::Select(s));
    }

    fn terminal_width(&self) -> u16 {
        if self.terminal_pane.width > 0 {
            self.terminal_pane.width
        } else {
            (self.size.0 / 2).max(24)
        }
    }

    fn open_terminal(&mut self) {
        let cols = self.terminal_width().saturating_sub(2).max(20);
        let rows = self.size.1.saturating_sub(4).max(6);
        let directory = self.handle.with(|s| s.directory.clone());
        match self.terminal_pane.open_new(&self.sid(), &directory, cols, rows) {
            Ok(_) => self.cache = None,
            Err(error) => self.toast(Variant::Error, None, format!("Terminal failed: {error}")),
        }
    }

    fn open_composer(&mut self, tab: usize) {
        self.composer_open = true;
        self.composer_tab = tab.min(2);
        self.composer_selected = 0;
        self.cache = None;
    }

    fn close_composer(&mut self) {
        self.composer_open = false;
        self.composer_selected = 0;
        self.cache = None;
    }

    fn composer_items(&self) -> Vec<(String, String)> {
        let mut items = Vec::new();
        match self.composer_tab {
            0 => {
                let sid = self.sid();
                let mut children: Vec<_> = crate::session::list(&self.app.project)
                    .into_iter()
                    .filter(|session| session.parent_id.as_deref() == Some(sid.as_str()))
                    .collect();
                children.sort_by_key(|session| session.time_updated);
                children.reverse();
                for child in children {
                    items.push((format!("session:{}", child.id), child.title));
                }
                if items.is_empty() {
                    items.push(("none".into(), "No subagents".into()));
                }
            }
            1 => {
                for (index, command) in self.shells.iter().enumerate() {
                    items.push((format!("shell:{index}"), command.clone()));
                }
                if items.is_empty() {
                    items.push(("none".into(), "No background shells".into()));
                }
            }
            _ => {
                for (index, title) in self.terminal_pane.tab_titles().into_iter().enumerate() {
                    items.push((format!("terminal:{index}"), title));
                }
                items.push(("terminal:new".into(), "+ New terminal".into()));
            }
        }
        items
    }

    fn composer_key(&mut self, k: &keymap::Key) {
        match k.name.as_str() {
            "escape" => self.close_composer(),
            "left" => {
                self.composer_tab = (self.composer_tab + 2) % 3;
                self.composer_selected = 0;
            }
            "right" => {
                self.composer_tab = (self.composer_tab + 1) % 3;
                self.composer_selected = 0;
            }
            "up" => {
                let n = self.composer_items().len().max(1);
                self.composer_selected = (self.composer_selected + n - 1) % n;
            }
            "down" => {
                let n = self.composer_items().len().max(1);
                self.composer_selected = (self.composer_selected + 1) % n;
            }
            "return" => {
                let items = self.composer_items();
                let Some((value, _)) = items.get(self.composer_selected.min(items.len().saturating_sub(1))) else { return };
                if let Some(id) = value.strip_prefix("session:") {
                    let id = id.to_string();
                    self.close_composer();
                    self.open_session(&id);
                } else if let Some(index) = value.strip_prefix("terminal:").and_then(|v| v.parse::<usize>().ok()) {
                    self.terminal_pane.active = index.min(self.terminal_pane.tabs.len().saturating_sub(1));
                    self.terminal_pane.visible = true;
                    self.terminal_pane.focus = true;
                    self.close_composer();
                } else if value == "terminal:new" {
                    self.close_composer();
                    self.open_terminal();
                } else if let Some(index) = value.strip_prefix("shell:").and_then(|v| v.parse::<usize>().ok()) {
                    if let Some(command) = self.shells.get(index).cloned() {
                        self.close_composer();
                        self.show_text("Shell", command);
                    }
                }
            }
            _ if k.ctrl && k.name == "c" => self.close_composer(),
            _ => {}
        }
    }

    fn stash_prompt(&mut self) {
        if self.editor.text.trim().is_empty() {
            self.toast(Variant::Info, None, "Nothing to stash");
            return;
        }
        self.stash.push(Stashed { text: self.editor.text.clone(), pasted: self.pasted.clone(), shell: self.shell });
        self.clear_prompt();
        self.toast(Variant::Success, None, "Prompt stashed");
    }

    fn pop_stash(&mut self) {
        let Some(stashed) = self.stash.pop() else {
            self.toast(Variant::Info, None, "No stashed prompts");
            return;
        };
        self.editor.set(&stashed.text);
        self.pasted = stashed.pasted;
        self.shell = stashed.shell;
        self.update_auto();
    }

    fn open_stash(&mut self) {
        if self.stash.is_empty() {
            self.toast(Variant::Info, None, "No stashed prompts");
            return;
        }
        let opts: Vec<dialog::Opt> = self
            .stash
            .iter()
            .enumerate()
            .map(|(index, stashed)| dialog::Opt {
                title: stashed.text.lines().next().unwrap_or("").chars().take(60).collect(),
                footer: format!("{} lines", stashed.text.lines().count()),
                value: index.to_string(),
                ..Default::default()
            })
            .collect();
        self.dialog = Some(Dialog::Select(dialog::Select::new(dialog::Kind::Stash, "Stashed prompts", opts)));
    }

    fn open_queued(&mut self) {
        let queued: Vec<String> = self.handle.queue.lock().unwrap().iter().map(|(message, _)| message.text()).collect();
        if queued.is_empty() {
            self.toast(Variant::Info, None, "No queued prompts");
            return;
        }
        let opts: Vec<dialog::Opt> = queued
            .iter()
            .enumerate()
            .map(|(index, text)| dialog::Opt {
                title: text.lines().next().unwrap_or("").chars().take(60).collect(),
                footer: format!("{} lines", text.lines().count()),
                value: index.to_string(),
                ..Default::default()
            })
            .collect();
        self.dialog = Some(Dialog::Select(dialog::Select::new(dialog::Kind::Queued, "Queued prompts", opts)));
    }

    fn goal_state(&self) -> Option<(String, String)> {
        self.handle.with(|session| {
            let goal = session.metadata.as_ref()?.get("goal")?;
            Some((
                goal.get("objective").and_then(Value::as_str).unwrap_or_default().to_string(),
                goal.get("status").and_then(Value::as_str).unwrap_or("active").to_string(),
            ))
        })
    }

    fn set_goal(&mut self, objective: &str, status: &str) {
        self.handle.with(|session| {
            let mut metadata = session.metadata.take().unwrap_or_else(|| json!({}));
            if !metadata.is_object() {
                metadata = json!({});
            }
            metadata["goal"] = json!({ "objective": objective, "status": status });
            session.metadata = Some(metadata);
        });
        self.handle.save();
    }

    fn goal_command(&mut self, args: &str) {
        if args.trim().is_empty() {
            self.open_goal();
            return;
        }
        self.set_goal(args.trim(), "active");
        self.toast(Variant::Success, None, "Goal set");
        self.cache = None;
    }

    fn open_goal(&mut self) {
        let Some((objective, status)) = self.goal_state() else {
            self.show_text("Goal", "No goal is set. Start one with /goal <objective>.".into());
            return;
        };
        let opts = vec![
            dialog::Opt { title: objective, footer: status.clone(), value: "show".into(), current: true, ..Default::default() },
            dialog::Opt { title: "Pause goal".into(), value: "pause".into(), ..Default::default() },
            dialog::Opt { title: "Resume goal".into(), value: "resume".into(), ..Default::default() },
            dialog::Opt { title: "Clear goal".into(), value: "clear".into(), ..Default::default() },
        ];
        self.dialog = Some(Dialog::Select(dialog::Select::new(dialog::Kind::Goal, "Goal", opts)));
    }

    fn pair_device(&mut self) {
        let status = crate::service::status();
        let url = status
            .get("url")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| std::env::var("OPENCODE_SERVER_URL").ok());
        let Some(url) = url else {
            self.show_text("Pair device", format!("No background server is running.\n\n{}", actions::service_hint()));
            return;
        };
        let password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
        let app = self.app.clone();
        tokio::spawn(async move {
            let text = match actions::pair(&url, password.as_deref()).await {
                Ok(link) => format!("Pairing link:\n\n{link}\n\nThe link can be used once and expires in five minutes."),
                Err(error) => format!("Pairing failed: {error}"),
            };
            app.emit(AppEvent::PluginPage { page: String::new(), title: "Pair device".into(), text, layout: Value::Null });
        });
    }

    fn restart_service(&mut self) {
        let app = self.app.clone();
        tokio::spawn(async move {
            let text = match actions::restart_service() {
                Ok(text) => text,
                Err(error) => format!("Restart failed: {error}"),
            };
            app.emit(AppEvent::PluginPage { page: String::new(), title: "Service".into(), text, layout: Value::Null });
        });
    }

    fn reload_config(&mut self) {
        match self.app.reload_config() {
            Ok(()) => {
                self.cache = None;
                self.toast(Variant::Success, None, "Configuration reloaded.");
            }
            Err(error) => self.toast(Variant::Error, None, format!("Configuration reload failed: {error}")),
        }
    }

    fn change_directory(&mut self, directory: &str) {
        match navigation::move_session(&self.handle, std::path::Path::new(directory)) {
            Ok(()) => {
                self.cache = None;
                self.toast(Variant::Success, None, "Working directory changed.");
            }
            Err(error) => self.toast(Variant::Error, None, format!("Change directory failed: {error}")),
        }
    }

    fn ask_btw(&mut self, question: String) {
        let app = self.app.clone();
        let sid = self.sid();
        tokio::spawn(async move {
            let text = match actions::btw(app.clone(), sid, question).await {
                Ok(text) => text,
                Err(error) => format!("Side question failed: {error}"),
            };
            app.emit(AppEvent::PluginPage { page: String::new(), title: "btw".into(), text, layout: Value::Null });
        });
    }

    fn open_provider_key(&mut self, value: &str) {
        let provider = value.strip_prefix("provider:").unwrap_or(value).to_string();
        self.dialog = Some(Dialog::Rename(dialog::Prompt {
            title: format!("API key for {provider}"),
            editor: Editor::default(),
            target: format!("connect:{provider}"),
        }));
    }

    fn reopen_settings(&mut self, value: &str) {
        self.open_settings();
        if let Some(Dialog::Select(s)) = self.dialog.as_mut() {
            if let Some(i) = s.visible().iter().position(|o| o.value == value) {
                s.selected = i;
            }
        }
    }

    fn dialog_select(&mut self, kind: Kind, value: String) {
        match kind {
            Kind::Palette => {
                self.dialog = None;
                match value.strip_prefix("slash:") {
                    Some(name) => self.editor.set(&format!("/{name} ")),
                    None => self.exec(&value, ""),
                }
            }
            Kind::Models => {
                self.dialog = None;
                self.set_model(&value, true);
                if self.variant.is_none() && self.current_model().is_some_and(|m| !m.variants().is_empty()) && !self.prefs.variant.contains_key(&value) {
                    self.open_variants();
                }
            }
            Kind::Sessions => {
                self.dialog = None;
                self.open_session(&value);
            }
            Kind::Agents => {
                self.dialog = None;
                self.agent = value;
                if let Some(m) = self.app.agent(&self.agent).ok().and_then(|a| a.model.clone()) {
                    self.pick_model(Some(m));
                }
            }
            Kind::Variants => {
                self.dialog = None;
                self.variant = if value == "default" { None } else { Some(value) };
                self.remember_variant();
            }
            Kind::Themes => {
                self.dialog = None;
                self.theme_before = None;
                if self.set_theme(&value) {
                    self.settings.set("theme.name", json!(value));
                }
            }
            Kind::Timeline => {
                self.dialog = None;
                let idx = self.handle.with(|s| s.messages.iter().filter(|m| m.role == Role::User && m.parts.iter().any(|p| matches!(p, Part::Text { synthetic: false, .. }))).position(|m| m.id == value));
                if let (Some(i), Some(c)) = (idx, &self.cache) {
                    if let Some(&start) = c.out.users.get(i) {
                        self.scroll = c.out.lines.len().saturating_sub(start + self.body_height);
                    }
                }
            }
            Kind::Fork => {
                self.dialog = None;
                let s = self.handle.session.lock().unwrap().clone();
                let mut f = crate::session::fork(&s);
                if let Some(i) = f.messages.iter().position(|m| m.id == value) {
                    let text = f.messages[i].text();
                    f.messages.truncate(i);
                    self.editor.set(&text);
                }
                let _ = crate::session::save(&f);
                let id = f.id.clone();
                self.open_session(&id);
            }
            Kind::Settings => {
                match value.as_str() {
                    "theme" => return self.open_themes(),
                    "thinking" => self.exec("session.toggle.thinking", ""),
                    "sidebar" => {
                        let next = if self.settings.sidebar_auto() { "hide" } else { "auto" };
                        self.settings.set("session.sidebar", json!(next));
                        self.sidebar = None;
                    }
                    "grouping" => self.exec("session.verbosity.cycle", ""),
                    "tps" => {
                        let v = !self.settings.tps();
                        self.settings.set("session.tps", json!(v));
                    }
                    "animations" => {
                        let v = !self.settings.animations();
                        self.settings.set("animations", json!(v));
                    }
                    "tabs.mode" => {
                        let next = match self.settings.tabs_mode() { "auto" => "on", "on" => "off", _ => "auto" };
                        self.settings.set("tabs.mode", json!(next));
                    }
                    "tabs.scope" => {
                        let next = if self.settings.tabs_scope() == "global" { "cwd" } else { "global" };
                        self.settings.set("tabs.scope", json!(next));
                        self.saved_tabs.clear();
                    }
                    "tabs.layout" => {
                        let next = if self.settings.tabs_layout() == "vertical" { "horizontal" } else { "vertical" };
                        self.settings.set("tabs.layout", json!(next));
                    }
                    "tabs.indicators" => {
                        let next = if self.settings.tabs_indicators() == "numbers" { "status" } else { "numbers" };
                        self.settings.set("tabs.indicators", json!(next));
                    }
                    _ => {}
                }
                self.cache = None;
                self.reopen_settings(&value);
            }
            Kind::Open => {
                self.dialog = None;
                self.open_session(&value);
            }
            Kind::Connect => {
                self.dialog = None;
                self.open_provider_key(&value);
            }
            Kind::Workspaces => {
                self.dialog = None;
                if value == "workspace-new" {
                    let mut editor = Editor::default();
                    editor.set(&self.handle.with(|session| session.directory.to_string_lossy().to_string()));
                    self.dialog = Some(Dialog::Rename(dialog::Prompt {
                        title: "Workspace directory".into(),
                        editor,
                        target: "workspace".into(),
                    }));
                } else {
                    match navigation::bind_workspace(&self.handle, &value) {
                        Ok(()) => {
                            self.cache = None;
                            self.toast(Variant::Success, None, "Workspace connected.");
                        }
                        Err(error) => self.toast(Variant::Error, None, format!("Workspace failed: {error}")),
                    }
                }
            }
            Kind::Skills => {
                self.dialog = None;
                let mention = format!("@{value} ");
                if !self.editor.text.split_whitespace().any(|word| word == format!("@{value}")) {
                    self.editor.insert(&mention);
                }
            }
            Kind::Stash => {
                self.dialog = None;
                if let Ok(index) = value.parse::<usize>() {
                    if let Some(stashed) = self.stash.get(index).cloned() {
                        self.editor.set(&stashed.text);
                        self.pasted = stashed.pasted;
                        self.shell = stashed.shell;
                        self.update_auto();
                    }
                }
            }
            Kind::Queued => {
                self.dialog = None;
                if let Ok(index) = value.parse::<usize>() {
                    let mut queue = self.handle.queue.lock().unwrap();
                    if index < queue.len() {
                        queue.remove(index);
                    }
                }
            }
            Kind::Goal => {
                self.dialog = None;
                let objective = self.goal_state().map(|(objective, _)| objective).unwrap_or_default();
                match value.as_str() {
                    "pause" => self.set_goal(&objective, "paused"),
                    "resume" => self.set_goal(&objective, "active"),
                    "clear" => {
                        self.handle.with(|session| {
                            if let Some(metadata) = session.metadata.as_mut().and_then(Value::as_object_mut) {
                                metadata.remove("goal");
                            }
                        });
                        self.handle.save();
                    }
                    _ => {}
                }
                self.cache = None;
            }
            Kind::TabMenu => {
                self.dialog = None;
                let target = self.tab_menu_target.take().unwrap_or_else(|| self.sid());
                match value.as_str() {
                    "close" => self.close_tab_id(&target),
                    "close_others" => self.close_tabs_to(&target, false),
                    "close_right" => self.close_tabs_to(&target, true),
                    "reopen" => self.reopen_tab(),
                    "rename" => {
                        let mut editor = Editor::default();
                        editor.set(&self.app.session(&target).map(|h| h.with(|s| s.title.clone())).unwrap_or_default());
                        self.dialog = Some(Dialog::Rename(dialog::Prompt {
                            title: "Rename Session".into(),
                            editor,
                            target,
                        }));
                    }
                    _ => {}
                }
            }
            Kind::Mcp => {
                self.dialog = None;
                let status = self
                    .app
                    .mcp
                    .snapshot()
                    .into_iter()
                    .find(|(n, _)| *n == value)
                    .and_then(|(_, status)| status);
                match status {
                    Some(crate::mcp::Status::NeedsAuth(_)) => self.start_mcp_auth(value),
                    Some(crate::mcp::Status::Failed(_)) => self.retry_mcp(value),
                    Some(crate::mcp::Status::Connected { .. }) => self.toast(Variant::Info, None, format!("{value} is already connected.")),
                    _ => self.toast(Variant::Info, None, format!("{value} is not connected.")),
                }
            }
        }
    }

    fn close_dialog(&mut self) {
        if let Some(prev) = self.theme_before.take() {
            self.set_theme(&prev);
        }
        if let Some(Dialog::Terminal(term)) = &self.dialog {
            term.close();
        }
        if let Some(Dialog::McpAuth(auth)) = &self.dialog {
            if let Some(handle) = &auth.handle {
                handle.cancel();
            }
        }
        self.dialog = None;
    }

    fn diff_key(&mut self, k: &Key) {
        let Some(Dialog::Diff(mut view)) = self.dialog.take() else { return };
        let mut reload = None;
        match k.name.as_str() {
            "escape" | "q" => {
                self.dialog = None;
                return;
            }
            "j" | "down" => view.scroll = view.scroll.saturating_add(1),
            "k" | "up" => view.scroll = view.scroll.saturating_sub(1),
            "pagedown" => view.scroll = view.scroll.saturating_add(12),
            "pageup" => view.scroll = view.scroll.saturating_sub(12),
            "g" | "home" => view.scroll = 0,
            "G" | "end" => view.scroll = usize::MAX,
            "]" => view.next_hunk(true),
            "[" => view.next_hunk(false),
            "n" => view.next_file(1),
            "p" => view.next_file(-1),
            "b" => view.tree = !view.tree,
            "s" => view.single = !view.single,
            "v" => view.split = !view.split,
            "f" => view.fullscreen = !view.fullscreen,
            "m" => view.toggle_reviewed(),
            "?" => view.help = !view.help,
            "d" => reload = Some(view.source.next().name().to_string()),
            _ if k.ctrl && k.name == "d" => view.scroll = view.scroll.saturating_add(6),
            _ if k.ctrl && k.name == "u" => view.scroll = view.scroll.saturating_sub(6),
            _ => {}
        }
        if let Some(source) = reload {
            self.open_diff_source(Some(&source));
        } else {
            self.dialog = Some(Dialog::Diff(view));
        }
    }

    fn dialog_key(&mut self, k: &Key, ev: &KeyEvent) {
        if let Some(Dialog::Image { scroll, .. }) = self.dialog.as_mut() {
            match k.name.as_str() {
                "escape" | "q" | "return" => self.dialog = None,
                "up" | "k" => *scroll = scroll.saturating_sub(1),
                "down" | "j" => *scroll = scroll.saturating_add(1),
                "pageup" => *scroll = scroll.saturating_sub(12),
                "pagedown" => *scroll = scroll.saturating_add(12),
                _ => {}
            }
            return;
        }
        if matches!(self.dialog, Some(Dialog::Diff(_))) {
            self.diff_key(k);
            return;
        }
        let plugin_app = self.app.clone();
        let act = {
            let km = &self.keys;
            match self.dialog.as_mut() {
                None => Act::None,
                Some(Dialog::Diff(_)) | Some(Dialog::Image { .. }) => Act::None,
                Some(Dialog::Help) | Some(Dialog::Status) => {
                    if k.name == "return" || k.name == "escape" || (k.ctrl && k.name == "c") {
                        Act::Close
                    } else {
                        Act::None
                    }
                }
                Some(Dialog::Plugin { page, layout, scroll, focus, .. }) => {
                    if !page.is_empty() {
                        let event = json!({
                            "name": k.name,
                            "ctrl": k.ctrl,
                            "alt": k.alt,
                            "shift": k.shift,
                            "sequence": format!("{:?}", ev.code),
                            "eventType": "press",
                            "repeated": false,
                        });
                        let app = plugin_app.clone();
                        tokio::spawn(async move {
                            let _ = app.plugins.tui_key(event).await;
                        });
                    }
                    let controls = plugin_ui::controls(layout);
                    match k.name.as_str() {
                        "escape" => Act::Close,
                        "return" if !page.is_empty() && !controls.is_empty() => {
                            let id = format!("tui-control-{focus}");
                            let value = controls
                                .get(*focus)
                                .map(|control| control.value.clone())
                                .unwrap_or_default();
                            let page = page.clone();
                            let app = plugin_app.clone();
                            tokio::spawn(async move {
                                if let Ok(result) = app.plugins.tui_event(&id, "submit", Value::String(value)).await {
                                    if result.get("handled").and_then(Value::as_bool) == Some(true) {
                                        app.emit(AppEvent::PluginInvalidate);
                                    }
                                }
                            });
                            Act::None
                        }
                        "tab" | "down" | "up" if !controls.is_empty() => {
                            let step = if k.name == "up" || (k.name == "tab" && k.shift) { -1 } else { 1 };
                            *focus = (*focus as isize + step).rem_euclid(controls.len() as isize) as usize;
                            Act::None
                        }
                        "up" if controls.is_empty() => {
                            *scroll = scroll.saturating_sub(1);
                            Act::None
                        }
                        "down" if controls.is_empty() => {
                            *scroll += 1;
                            Act::None
                        }
                        "pageup" => {
                            *scroll = scroll.saturating_sub(12);
                            Act::None
                        }
                        "pagedown" => {
                            *scroll += 12;
                            Act::None
                        }
                        "left" | "right" => {
                            if let Some(control) = controls.get(*focus) {
                                if control.kind == plugin_ui::ControlKind::Select && !control.options.is_empty() {
                                    let step = if k.name == "left" { -1 } else { 1 };
                                    let next = (control.selected as isize + step)
                                        .rem_euclid(control.options.len() as isize)
                                        as usize;
                                    plugin_ui::set_control_selected(layout, *focus, next);
                                    if !page.is_empty() {
                                        let id = format!("tui-control-{focus}");
                                        let value = control.options.get(next).cloned().unwrap_or_default();
                                        let app = plugin_app.clone();
                                        tokio::spawn(async move {
                                            if let Ok(result) = app.plugins.tui_event(&id, "select", Value::String(value)).await {
                                                if result.get("handled").and_then(Value::as_bool) == Some(true) {
                                                    app.emit(AppEvent::PluginInvalidate);
                                                }
                                            }
                                        });
                                    }
                                } else if control.kind == plugin_ui::ControlKind::Input {
                                    let step = if k.name == "left" { -1 } else { 1 };
                                    let cursor = (control.cursor as isize + step)
                                        .clamp(0, control.value.chars().count() as isize)
                                        as usize;
                                    plugin_ui::set_control_cursor(layout, *focus, cursor);
                                }
                            }
                            Act::None
                        }
                        "home" | "end" => {
                            if let Some(control) = controls.get(*focus) {
                                if control.kind == plugin_ui::ControlKind::Input {
                                    let cursor = if k.name == "home" { 0 } else { control.value.chars().count() };
                                    plugin_ui::set_control_cursor(layout, *focus, cursor);
                                }
                            }
                            Act::None
                        }
                        "backspace" => {
                            if let Some(control) = controls.get(*focus) {
                                if control.kind == plugin_ui::ControlKind::Input {
                                    let mut chars: Vec<char> = control.value.chars().collect();
                                    if control.cursor > 0 && control.cursor <= chars.len() {
                                        chars.remove(control.cursor - 1);
                                    }
                                    let value: String = chars.into_iter().collect();
                                    let cursor = control.cursor.saturating_sub(1);
                                    plugin_ui::set_control_value_at(layout, *focus, value.clone(), cursor);
                                    if !page.is_empty() {
                                        let id = format!("tui-control-{focus}");
                                        let app = plugin_app.clone();
                                        tokio::spawn(async move {
                                            if let Ok(result) = app.plugins.tui_event(&id, "input", Value::String(value)).await {
                                                if result.get("handled").and_then(Value::as_bool) == Some(true) {
                                                    app.emit(AppEvent::PluginInvalidate);
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                            Act::None
                        }
                        _ if k.ctrl && k.name == "c" => Act::Close,
                        _ => {
                            if let KeyCode::Char(c) = ev.code {
                                if !k.ctrl && !k.alt {
                                    if let Some(control) = controls.get(*focus) {
                                        if control.kind == plugin_ui::ControlKind::Input {
                                            let mut chars: Vec<char> = control.value.chars().collect();
                                            let cursor = control.cursor.min(chars.len());
                                            chars.insert(cursor, c);
                                            let value: String = chars.into_iter().collect();
                                            plugin_ui::set_control_value_at(layout, *focus, value.clone(), cursor + 1);
                                            if !page.is_empty() {
                                                let id = format!("tui-control-{focus}");
                                                let app = plugin_app.clone();
                                                tokio::spawn(async move {
                                                    if let Ok(result) = app.plugins.tui_event(&id, "input", Value::String(value)).await {
                                                        if result.get("handled").and_then(Value::as_bool) == Some(true) {
                                                            app.emit(AppEvent::PluginInvalidate);
                                                        }
                                                    }
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            Act::None
                        }
                    }
                }
                Some(Dialog::Text { scroll, .. }) => match k.name.as_str() {
                    "escape" | "return" => Act::Close,
                    "up" => {
                        *scroll = scroll.saturating_sub(1);
                        Act::None
                    }
                    "down" => {
                        *scroll += 1;
                        Act::None
                    }
                    "pageup" => {
                        *scroll = scroll.saturating_sub(12);
                        Act::None
                    }
                    "pagedown" => {
                        *scroll += 12;
                        Act::None
                    }
                    _ if k.ctrl && k.name == "c" => Act::Close,
                    _ => Act::None,
                },
                Some(Dialog::Terminal(t)) => {
                    if k.name == "escape" {
                        Act::Close
                    } else {
                        let _ = t.key(ev);
                        Act::None
                    }
                }
                Some(Dialog::McpAuth(a)) => match k.name.as_str() {
                    "escape" => Act::Close,
                    "return" => {
                        let text = a.editor.text.trim().to_string();
                        if text.is_empty() {
                            Act::McpAuthOpen(a.url.clone())
                        } else {
                            Act::McpAuthSubmit(text)
                        }
                    }
                    "c" if a.editor.text.is_empty() => Act::McpAuthCopy(a.url.clone()),
                    _ => {
                        edit_key(&mut a.editor, k, ev, km, false);
                        Act::None
                    }
                },
                Some(Dialog::Rename(p)) => match k.name.as_str() {
                    "escape" => Act::Close,
                    "return" => Act::Rename(p.target.clone(), p.editor.text.trim().to_string()),
                    _ => {
                        edit_key(&mut p.editor, k, ev, km, false);
                        Act::None
                    }
                },
                Some(Dialog::Select(s)) => {
                    let kind = s.kind;
                    if k.name == "escape" || (k.ctrl && k.name == "c" && s.filter.text.is_empty()) {
                        Act::Close
                    } else if k.ctrl && k.name == "c" {
                        s.filter.clear();
                        s.set_filter();
                        Act::None
                    } else if km.matches("dialog.select.prev", k, false) {
                        s.moved(-1);
                        Act::None
                    } else if km.matches("dialog.select.next", k, false) {
                        s.moved(1);
                        Act::None
                    } else if km.matches("dialog.select.page_up", k, false) {
                        s.moved(-10);
                        Act::None
                    } else if km.matches("dialog.select.page_down", k, false) {
                        s.moved(10);
                        Act::None
                    } else if km.matches("dialog.select.home", k, false) && s.filter.text.is_empty() {
                        s.selected = 0;
                        Act::None
                    } else if km.matches("dialog.select.end", k, false) && s.filter.text.is_empty() {
                        s.selected = s.visible().len().saturating_sub(1);
                        Act::None
                    } else if km.matches("dialog.select.submit", k, false) {
                        s.current().map(|o| Act::Pick(kind, o.value.clone())).unwrap_or(Act::None)
                    } else if kind == Kind::Sessions && km.matches("session.delete", k, false) {
                        match s.current().map(|o| o.value.clone()) {
                            Some(v) if s.confirm.as_deref() == Some(v.as_str()) => {
                                s.confirm = None;
                                Act::Delete(v)
                            }
                            Some(v) => {
                                s.confirm = Some(v);
                                Act::None
                            }
                            None => Act::None,
                        }
                    } else if kind == Kind::Sessions && km.matches("session.rename", k, false) {
                        s.current().map(|o| Act::RenameOpen(o.value.clone(), o.title.clone())).unwrap_or(Act::None)
                    } else if kind == Kind::Sessions && k.ctrl && k.name == "a" {
                        Act::Sessions(!s.all)
                    } else if kind == Kind::Models && km.matches("model.dialog.favorite", k, false) {
                        s.current().map(|o| Act::Favorite(o.value.clone(), s.selected)).unwrap_or(Act::None)
                    } else if kind == Kind::Models && km.matches("model.dialog.provider", k, false) {
                        Act::Provider
                    } else {
                        let before = s.filter.text.clone();
                        edit_key(&mut s.filter, k, ev, km, false);
                        if s.filter.text != before {
                            s.set_filter();
                        }
                        Act::None
                    }
                }
            }
        };
        match act {
            Act::None => {}
            Act::Close => self.close_dialog(),
            Act::Pick(kind, v) => self.dialog_select(kind, v),
            Act::McpAuthOpen(url) => match crate::mcp_oauth::open_browser(&url) {
                Ok(()) => self.toast(Variant::Info, None, "Opened the sign-in page in your browser."),
                Err(_) => self.toast(Variant::Error, None, "Could not open the browser. Copy the URL and continue manually."),
            },
            Act::McpAuthCopy(url) => {
                copy_to_clipboard(&url);
                self.toast(Variant::Info, None, "Copied to clipboard");
            }
            Act::McpAuthSubmit(value) => {
                let result = match self.dialog.as_mut() {
                    Some(Dialog::McpAuth(auth)) => match &auth.handle {
                        Some(handle) => handle.submit(&value),
                        None => Err(anyhow::anyhow!("this sign-in attempt is no longer available")),
                    },
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    if let Some(Dialog::McpAuth(auth)) = self.dialog.as_mut() {
                        auth.error = Some(error.to_string());
                    }
                }
            }
            Act::Rename(target, title) => {
                self.dialog = None;
                if let Some(provider) = target.strip_prefix("connect:") {
                    match actions::save_provider_key(provider, &title) {
                        Ok(()) => {
                            *self.app.providers.write().unwrap() = crate::models::providers(&self.app.config());
                            self.toast(Variant::Success, None, format!("API key saved for {provider}"));
                        }
                        Err(error) => self.toast(Variant::Error, None, format!("Could not save key: {error}")),
                    }
                } else if target == "cd" {
                    if !title.is_empty() {
                        self.change_directory(&title);
                    }
                } else if target == "btw" {
                    if !title.is_empty() {
                        self.ask_btw(title);
                    }
                } else if target == "workspace" {
                    if !title.is_empty() {
                        let base = self.handle.with(|session| session.directory.clone());
                        match navigation::create_workspace(std::path::Path::new(&title), &base) {
                            Ok(id) => match navigation::bind_workspace(&self.handle, &id) {
                                Ok(()) => {
                                    self.cache = None;
                                    self.toast(Variant::Success, None, "Workspace created and connected.");
                                }
                                Err(error) => self.toast(Variant::Error, None, format!("Workspace failed: {error}")),
                            },
                            Err(error) => self.toast(Variant::Error, None, format!("Workspace creation failed: {error}")),
                        }
                    }
                } else if !title.is_empty() {
                    self.rename(&target, &title);
                }
            }
            Act::RenameOpen(target, title) => {
                let mut e = Editor::default();
                e.set(&title);
                self.dialog = Some(Dialog::Rename(dialog::Prompt { title: "Rename Session".into(), editor: e, target }));
            }
            Act::Sessions(all) => self.open_sessions(all),
            Act::Delete(v) => {
                let project = crate::session::load(&self.app.project, &v).map(|x| x.project).unwrap_or_else(|_| self.app.project.clone());
                let _ = crate::session::delete(&project, &v);
                self.app.forget_session(&v);
                self.tabs.retain(|t| *t != v);
                if v == self.handle.id() {
                    self.handle = self.app.new_session(None);
                    self.route = Route::Home;
                    self.cache = None;
                }
                if let Some(Dialog::Select(s)) = self.dialog.as_mut() {
                    s.options.retain(|o| o.value != v);
                    s.selected = s.selected.min(s.visible().len().saturating_sub(1));
                }
            }
            Act::Favorite(v, sel) => {
                match self.prefs.favorite.iter().position(|f| *f == v) {
                    Some(i) => {
                        self.prefs.favorite.remove(i);
                    }
                    None => self.prefs.favorite.insert(0, v),
                }
                self.prefs.save();
                self.open_models();
                if let Some(Dialog::Select(n)) = self.dialog.as_mut() {
                    n.selected = sel.min(n.visible().len().saturating_sub(1));
                }
            }
            Act::Provider => {
                self.dialog = None;
                self.info("Run `opencode auth login` to connect a provider");
            }
        }
        // the theme list previews the highlighted theme
        let preview = match self.dialog.as_ref() {
            Some(Dialog::Select(s)) if s.kind == Kind::Themes => s.current().map(|o| o.value.clone()),
            _ => None,
        };
        if let Some(v) = preview {
            if v != self.theme.name {
                self.set_theme(&v);
            }
        }
    }

    // ---------------------------------------------------------------- keys

    fn on_key(&mut self, ev: KeyEvent) {
        let Some(k) = keymap::from_event(&ev) else { return };
        let in_session = self.route == Route::Session;
        let leader = self.leader.take().is_some_and(|t| t.elapsed() < LEADER_TIMEOUT);
        if !leader && self.keys.is_leader(&k) && self.dialog.is_none() && self.perm.rejecting.is_none() {
            self.leader = Some(Instant::now());
            return;
        }
        if self.composer_open && !leader && self.dialog.is_none() {
            self.composer_key(&k);
            return;
        }
        if in_session && !self.handle.queue.lock().unwrap().is_empty() && self.keys.matches("queued_prompt.undo", &k, false) {
            self.exec("queued_prompt.undo", "");
            return;
        }
        if in_session && !self.handle.queue.lock().unwrap().is_empty() && self.keys.matches("queued_prompt.delete", &k, false) {
            self.exec("queued_prompt.delete", "");
            return;
        }
        if in_session && self.editor.text.is_empty() && !self.composer_open {
            if self.keys.matches("session.child.next", &k, false) {
                self.exec("session.child.next", "");
                return;
            }
            if self.keys.matches("session.child.previous", &k, false) {
                self.exec("session.child.previous", "");
                return;
            }
            if self.keys.matches("session.parent", &k, false) {
                self.exec("session.parent", "");
                return;
            }
        }
        if in_session && self.keys.matches("session.child.first", &k, false) && self.editor.text.is_empty() {
            if self.composer_open { self.close_composer() } else { self.open_composer(0) }
            return;
        }
        if self.terminal_pane.visible && self.terminal_pane.focus && self.dialog.is_none() {
            if k.name == "escape" {
                self.terminal_pane.focus = false;
                return;
            }
            if self.terminal_pane.key(&ev) {
                return;
            }
        }
        if self.dialog.is_some() {
            if leader {
                for id in ["session.list", "session.new"] {
                    if self.keys.matches(id, &k, true) {
                        self.dialog = None;
                        self.exec(id, "");
                        return;
                    }
                }
            }
            self.dialog_key(&k, &ev);
            return;
        }
        if leader {
            if let Some(id) = self.plugin_binding(&k, true) {
                self.exec(&format!("tui:{id}"), "");
                return;
            }
            let ids = [
                "app.exit", "prompt.editor", "session.sidebar.toggle", "opencode.status", "session.export", "session.new", "session.list", "session.timeline", "session.compact", "session.queued_prompts", "model.list",
                "agent.list", "messages.copy", "session.undo", "session.redo", "terminal.toggle", "terminal.select", "terminal.close",
            ];
            let session_only = ["session.timeline", "session.compact", "messages.copy", "session.undo", "session.redo", "session.export", "terminal.toggle", "terminal.select", "terminal.close"];
            if let Some(id) = TAB_COMMANDS.into_iter().find(|id| self.keys.matches(id, &k, true)) {
                self.exec(id, "");
                return;
            }
            if let Some(id) = ids.into_iter().find(|id| self.keys.matches(id, &k, true)) {
                if id == "app.exit" && !self.editor.text.is_empty() {
                    return;
                }
                if self.route == Route::Home && session_only.contains(&id) {
                    return;
                }
                self.exec(id, "");
            }
            return;
        }
        if !self.permissions.is_empty() {
            self.permission_key(&k, &ev);
            return;
        }
        if self.pending_form().is_some() {
            self.form_key(&k, &ev);
            return;
        }
        if let Some(a) = self.auto.as_mut() {
            let n = a.options.len();
            let km = &self.keys;
            if km.matches("prompt.autocomplete.prev", &k, false) && n > 0 {
                a.selected = (a.selected + n - 1) % n;
                return;
            }
            if km.matches("prompt.autocomplete.next", &k, false) && n > 0 {
                a.selected = (a.selected + 1) % n;
                return;
            }
            if km.matches("prompt.autocomplete.hide", &k, false) {
                self.auto = None;
                return;
            }
            if (km.matches("prompt.autocomplete.select", &k, false) || km.matches("prompt.autocomplete.complete", &k, false)) && n > 0 {
                let opt = a.options[a.selected].clone();
                let complete = k.name == "tab";
                self.pick_auto(opt, complete);
                return;
            }
        }
        if let Some(id) = self.plugin_binding(&k, false) {
            self.exec(&format!("tui:{id}"), "");
            return;
        }
        let in_session = self.route == Route::Session;
        if let Some(id) = TAB_COMMANDS.into_iter().find(|id| self.keys.matches(id, &k, false)) {
            self.exec(id, "");
            return;
        }
        let global = [
            "command.palette.show", "agent.cycle", "agent.cycle.reverse", "variant.cycle", "model.cycle_recent", "model.cycle_recent_reverse", "session.page.up", "session.page.down",
            "session.line.up", "session.line.down", "session.half.page.up", "session.half.page.down", "session.first", "session.last", "session.messages_last_user",
        ];
        if let Some(id) = global.into_iter().find(|id| self.keys.matches(id, &k, false)) {
            if !id.starts_with("session.") || in_session {
                self.exec(id, "");
                return;
            }
        }
        if in_session && self.keys.matches("session.rename", &k, false) {
            self.exec("session.rename", "");
            return;
        }
        if self.keys.matches("session.tab.next_unread", &k, false) {
            self.cycle_unread(1);
            return;
        }
        if self.keys.matches("session.tab.previous_unread", &k, false) {
            self.cycle_unread(-1);
            return;
        }
        if self.keys.matches("session.tab.history.back", &k, false) {
            self.tab_history_step(false);
            return;
        }
        if self.keys.matches("session.tab.history.forward", &k, false) {
            self.tab_history_step(true);
            return;
        }
        if in_session && self.keys.matches("prompt.skills", &k, false) {
            self.exec("prompt.skills", "");
            return;
        }
        if k.name == "escape" {
            if self.shell {
                self.shell = false;
            } else if self.handle.is_busy() {
                self.interrupt_press();
            }
            return;
        }
        if self.keys.matches("prompt.clear", &k, false) && !self.editor.text.is_empty() {
            self.clear_prompt();
            return;
        }
        if k.ctrl && k.name == "c" && self.shell {
            self.shell = false;
            return;
        }
        if self.keys.matches("app.exit", &k, false) && self.editor.text.is_empty() {
            self.quit = true;
            return;
        }
        if self.keys.matches("input.submit", &k, false) {
            if self.editor.text.ends_with('\\') {
                self.editor.backspace();
                self.editor.insert("\n");
            } else {
                self.submit();
            }
            return;
        }
        if !self.shell && self.editor.cursor == 0 && ev.code == KeyCode::Char('!') && !k.ctrl && !k.alt {
            self.shell = true;
            self.placeholder = (self.placeholder + 1) % 3;
            return;
        }
        if self.shell && self.editor.cursor == 0 && k.name == "backspace" {
            self.shell = false;
            return;
        }
        let width = self.prompt_text_width();
        if self.keys.matches("prompt.history.previous", &k, false) {
            let (row, _) = self.editor.row(width);
            if row > 0 {
                self.editor.vertical(width, false);
            } else if self.editor.cursor != 0 {
                self.editor.cursor = 0;
            } else if let Some((t, shell, pasted)) = self.history.step(-1, &self.editor.text.clone()) {
                self.editor.set(&t);
                self.editor.cursor = 0;
                self.shell = shell;
                self.pasted = pasted;
            }
            return;
        }
        if self.keys.matches("prompt.history.next", &k, false) {
            let (row, rows) = self.editor.row(width);
            if row + 1 < rows {
                self.editor.vertical(width, true);
            } else if self.editor.cursor != self.editor.text.len() {
                self.editor.cursor = self.editor.text.len();
            } else if let Some((t, shell, pasted)) = self.history.step(1, &self.editor.text.clone()) {
                self.editor.set(&t);
                self.shell = shell;
                self.pasted = pasted;
            }
            return;
        }
        edit_key(&mut self.editor, &k, &ev, &self.keys, true);
        self.update_auto();
    }

    fn session_width(&self) -> usize {
        let w = self.size.0 as usize;
        if self.sidebar_visible() && w > 120 {
            w - 42
        } else {
            w
        }
    }

    fn prompt_text_width(&self) -> usize {
        let w = self.size.0 as usize;
        let pad = if w < 44 { 1 } else { 2 };
        let outer = if self.route == Route::Home { 75.min(w.saturating_sub(4)) } else { self.session_width().saturating_sub(pad * 2) };
        outer.saturating_sub(1 + pad * 2).max(1)
    }

    fn update_auto(&mut self) {
        let Some((kind, start, query)) = prompt::trigger(&self.editor.text, self.editor.cursor) else {
            self.auto = None;
            return;
        };
        let prev = self.auto.as_ref().filter(|a| a.kind == kind && a.start == start).map(|a| a.selected).unwrap_or(0);
        let options: Vec<prompt::Option_> = match kind {
            AutoKind::Command => {
                let cmds = self.slash_commands();
                let max = cmds.iter().map(|(d, _, _)| d.chars().count()).max().unwrap_or(0);
                let mut scored: Vec<(i64, prompt::Option_)> = cmds
                    .into_iter()
                    .filter_map(|(display, desc, pick)| {
                        let score = if query.is_empty() {
                            0
                        } else {
                            let name = display.trim_start_matches('/');
                            let a = prompt::fuzzy(name, &query).map(|s| if name.starts_with(&query) { s * 2 } else { s });
                            let b = prompt::fuzzy(&desc, &query).map(|s| s / 2);
                            a.into_iter().chain(b).max()?
                        };
                        Some((score, prompt::Option_ { display: format!("{display:<w$}", w = max + 2), description: desc, label: "", value: display, action: pick }))
                    })
                    .collect();
                if !query.is_empty() {
                    scored.sort_by(|a, b| b.0.cmp(&a.0));
                    scored.truncate(10);
                }
                scored.into_iter().map(|(_, o)| o).collect()
            }
            AutoKind::Reference => {
                let mut non_files: Vec<prompt::Option_> = Vec::new();
                for s in self.app.skills() {
                    non_files.push(prompt::Option_ { display: format!("@{}", s.id), description: s.description.clone(), label: "skill", value: s.id.clone(), action: Pick::Mention(s.id.clone()) });
                }
                let agents = self.app.agents();
                let mut subs: Vec<&crate::agent::Agent> = agents.values().filter(|a| !a.hidden && a.is_subagent() && !a.is_primary()).collect();
                subs.sort_by_key(|a| a.id.clone());
                for a in subs {
                    non_files.push(prompt::Option_ { display: format!("@{}", a.id), description: String::new(), label: "agent", value: a.id.clone(), action: Pick::Mention(a.id.clone()) });
                }
                if !query.is_empty() {
                    let mut scored: Vec<(i64, prompt::Option_)> = non_files.into_iter().filter_map(|o| prompt::fuzzy(&o.value, &query).filter(|s| *s > 0).map(|s| (s, o))).collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0));
                    non_files = scored.into_iter().take(10).map(|(_, o)| o).collect();
                }
                let width = self.prompt_text_width() + 1;
                let mut options = non_files;
                let directory = self.cwd();
                for (p, _) in prompt::find_files(&directory, &query, 20) {
                    options.push(prompt::Option_ { display: draw::truncate_middle(&p, width.saturating_sub(4)), description: String::new(), label: "", value: p.clone(), action: Pick::Mention(p) });
                }
                options
            }
        };
        let selected = prev.min(options.len().saturating_sub(1));
        self.auto = Some(Auto { kind, start, selected, options });
    }

    fn pick_auto(&mut self, opt: prompt::Option_, complete: bool) {
        let Some(a) = self.auto.take() else { return };
        match opt.action {
            Pick::Command(id) if !complete => {
                self.editor.clear();
                self.exec(&id, "");
            }
            Pick::Command(_) | Pick::Slash(_) => {
                let name = opt.value.trim_start_matches('/').to_string();
                self.editor.set(&format!("/{name} "));
            }
            Pick::Mention(v) => {
                let end = self.editor.cursor;
                let insert = format!("@{v} ");
                self.editor.text.replace_range(a.start..end, &insert);
                self.editor.cursor = a.start + insert.len();
            }
        }
    }

    async fn refresh_plugin_slots(&mut self) -> bool {
        if self.plugin_slots_at.elapsed() < Duration::from_secs(2) {
            return false;
        }
        self.plugin_slots_at = Instant::now();
        let input = json!({
            "sessionID": self.sid(),
            "directory": self.cwd().to_string_lossy(),
            "mode": if self.shell { "shell" } else { "normal" },
            "showDetails": true,
            "width": self.size.0,
            "height": self.size.1,
        });
        let slots = self.app.plugins.tui_slots(input).await;
        let mut status = String::new();
        let mut sidebar = Vec::new();
        for slot in slots {
            let placement = slot.get("placement").and_then(Value::as_str).unwrap_or_default();
            let plain = slot
                .get("layout")
                .map(|layout| layout_plain_lines(layout, 0))
                .unwrap_or_default();
            let layout = slot.get("layout").cloned();
            let text = if plain.is_empty() {
                slot.get("text").and_then(Value::as_str).unwrap_or_default().to_string()
            } else {
                plain.join("
")
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match placement {
                "home.footer.status" | "prompt.footer.status" | "prompt.footer" | "home.footer" => {
                    if !status.is_empty() {
                        status.push_str("  ·  ");
                    }
                    status.push_str(&text.replace('\n', " "));
                }
                "sidebar.content" | "sidebar.footer" => {
                    sidebar.push(layout.unwrap_or_else(|| json!({ "type": "text", "text": text })));
                }
                _ => {}
            }
        }
        let changed = self.plugin_status != status || self.plugin_sidebar != sidebar;
        self.plugin_status = status;
        self.plugin_sidebar = sidebar;
        changed
    }

    fn plugin_binding(&self, key: &keymap::Key, leader: bool) -> Option<String> {
        for command in self.app.plugins.tui_commands() {
            let Some(id) = command.get("id").and_then(Value::as_str) else { continue };
            let Some(bind) = command.get("bind").and_then(Value::as_str) else { continue };
            if keymap::parse(bind).iter().any(|binding| binding.leader == leader && binding.key == *key) {
                return Some(id.to_string());
            }
        }
        None
    }

    fn pending_form(&self) -> Option<crate::forms::Form> {
        self.app
            .forms
            .list(&self.sid())
            .into_iter()
            .next()
            .or_else(|| self.app.forms.list("tui").into_iter().next())
    }

    fn form_key(&mut self, k: &Key, ev: &KeyEvent) {
        let Some(form) = self.pending_form() else { return };
        self.form.sync(&form);
        if k.name == "escape" || (k.ctrl && k.name == "c") {
            if let Err(e) = self.app.forms.cancel(&form.id) {
                self.toast(Variant::Error, None, e.to_string());
            }
            self.form = forms::FormUi::default();
            return;
        }
        if k.name == "return" {
            match self.form.answer() {
                Ok(answer) => match self.app.forms.reply(&form.id, answer) {
                    Ok(_) => self.form = forms::FormUi::default(),
                    Err(e) => self.toast(Variant::Error, None, e.to_string()),
                },
                Err(e) => self.toast(Variant::Error, None, e),
            }
            return;
        }
        if k.name == "tab" {
            self.form.next(if k.shift { -1 } else { 1 });
            return;
        }
        if k.name == "down" && !k.ctrl && !k.alt {
            self.form.next(1);
            return;
        }
        if k.name == "up" && !k.ctrl && !k.alt {
            self.form.next(-1);
            return;
        }
        if k.name == "space" && !k.ctrl && !k.alt && self.form.toggle() {
            return;
        }
        if let Some(editor) = self.form.editor() {
            edit_key(editor, k, ev, &self.keys, false);
        }
    }

    fn permission_options(req: &permission::Request) -> Vec<(&'static str, Reply)> {
        if req.save.is_empty() {
            vec![("Allow once", Reply::Once), ("Reject", Reply::Reject)]
        } else {
            vec![("Allow once", Reply::Once), ("Always allow", Reply::Always), ("Reject", Reply::Reject)]
        }
    }

    fn permission_key(&mut self, k: &Key, ev: &KeyEvent) {
        let Some(req) = self.permissions.front().cloned() else { return };
        let child = self.handle.id() != req.session_id;
        if let Some(e) = self.perm.rejecting.as_mut() {
            match k.name.as_str() {
                "escape" => self.perm.rejecting = None,
                "return" => {
                    self.perm.rejecting = None;
                    self.reply(&req, Reply::Reject);
                }
                _ if k.ctrl && k.name == "c" => {
                    if e.text.is_empty() {
                        self.perm.rejecting = None;
                    } else {
                        e.clear();
                    }
                }
                _ => edit_key(e, k, ev, &self.keys, false),
            }
            return;
        }
        let options = Self::permission_options(&req);
        let n = options.len();
        match k.name.as_str() {
            "left" | "h" if !k.ctrl => self.perm.selected = (self.perm.selected + n - 1) % n,
            "right" | "l" if !k.ctrl => self.perm.selected = (self.perm.selected + 1) % n,
            "return" => {
                let r = options[self.perm.selected.min(n - 1)].1;
                if r == Reply::Reject && child {
                    self.perm.rejecting = Some(Editor::default());
                    return;
                }
                self.reply(&req, r);
            }
            "escape" => {
                if self.perm.expanded {
                    self.perm.expanded = false;
                } else {
                    self.reply(&req, Reply::Reject);
                }
            }
            _ if self.keys.matches("permission.prompt.fullscreen", k, false) => self.perm.expanded = !self.perm.expanded,
            _ if k.ctrl && (k.name == "c" || k.name == "d") => self.reply(&req, Reply::Reject),
            _ => {}
        }
    }

    fn reply(&mut self, req: &permission::Request, r: Reply) {
        self.permissions.retain(|p| p.id != req.id);
        self.perm = PermUi::default();
        self.app.reply_permission(&req.id, r);
    }

    fn scroll_by(&mut self, d: isize) {
        self.scroll = (self.scroll as isize + d).max(0) as usize;
        if d > 0 {
            if let Some(cache) = self.cache.as_mut() {
                cache.older_loaded = false;
            }
        }
    }

    // ---------------------------------------------------------------- mouse

    fn on_mouse(&mut self, m: event::MouseEvent) {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if self.dialog.is_none() && self.permissions.is_empty() && self.pending_form().is_some() {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(m.kind, MouseEventKind::ScrollUp);
                if let Some(Dialog::Select(s)) = self.dialog.as_mut() {
                    s.moved(if up { -1 } else { 1 });
                    return;
                }
                if let Some(a) = self.auto.as_mut() {
                    let n = a.options.len().max(1);
                    a.selected = if up { (a.selected + n - 1) % n } else { (a.selected + 1) % n };
                    return;
                }
                self.scroll_by(if up { 3 } else { -3 });
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if self.dialog.is_none() {
                    if let Some(id) = self.tab_at(x, y).filter(|id| id != "+") {
                        self.open_tab_menu(id);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if self.dialog.is_none() {
                    if let Some(id) = self.tab_at(x, y).filter(|id| id != "+") {
                        self.close_tab_id(&id);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.dialog.is_none() {
                    if self.terminal_pane.visible && self.hits.terminal_pane.width > 0 && x + 1 == self.hits.terminal_pane.x {
                        self.terminal_resizing = true;
                        return;
                    }
                    if self.composer_open && inside(self.hits.composer_panel) {
                        if let Some((_, index)) = self.hits.composer_tabs.iter().find(|(rect, _)| inside(*rect)).copied() {
                            self.composer_tab = index;
                            self.composer_selected = 0;
                            return;
                        }
                        if let Some((_, index)) = self.hits.composer_rows.iter().find(|(rect, _)| inside(*rect)).copied() {
                            self.composer_selected = index;
                            let key = keymap::Key { ctrl: false, alt: false, shift: false, name: "return".into() };
                            self.composer_key(&key);
                            return;
                        }
                        return;
                    }
                    if self.terminal_pane.visible && inside(self.hits.terminal_pane) {
                        if let Some((_, index)) = self.hits.terminal_tabs.iter().find(|(rect, _)| inside(*rect)).copied() {
                            self.terminal_pane.active = index;
                            self.terminal_pane.focus = true;
                            return;
                        }
                        if self.hits.terminal_close.iter().any(|(rect, _)| inside(*rect)) {
                            self.terminal_pane.close_active();
                            self.cache = None;
                            return;
                        }
                        self.terminal_pane.focus = true;
                        return;
                    }
                    if let Some(id) = self.tab_at(x, y).filter(|id| id != "+") {
                        let index = self.tabs.iter().position(|t| *t == id).unwrap_or(0);
                        self.tab_drag = Some((id, index));
                        self.tab_drag_moved = false;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.terminal_resizing {
                    let max = self.size.0.saturating_sub(24).max(20);
                    self.terminal_pane.width = self.size.0.saturating_sub(x).clamp(20, max);
                    return;
                }
                if let Some((source, _)) = self.tab_drag.clone() {
                    if let Some(target) = self.tab_at(x, y).filter(|id| id != "+") {
                        if let Some(index) = self.tabs.iter().position(|t| *t == target) {
                            self.move_tab(&source, index);
                        }
                    }
                }
            }
            MouseEventKind::Moved => {
                let hover = if self.dialog.is_none() { self.tab_at(x, y) } else { None };
                if self.tab_hover != hover {
                    self.tab_hover = hover;
                    self.tab_hover_at = self.tab_hover.as_ref().map(|_| Instant::now());
                }
                if let (Some(Dialog::Select(s)), Some(p)) = (self.dialog.as_mut(), self.hits.dialog.as_ref()) {
                    if let Some((_, i)) = p.rows.iter().find(|(ry, _)| *ry == y) {
                        if inside(p.list) && s.selected != *i {
                            s.selected = *i;
                            s.confirm = None;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let moved = self.tab_drag_moved;
                self.tab_drag = None;
                self.tab_drag_moved = false;
                self.terminal_resizing = false;
                if moved {
                    self.persist_tabs();
                    return;
                }
                if self.terminal_pane.focus && !inside(self.hits.terminal_pane) {
                    self.terminal_pane.focus = false;
                }
                if self.composer_open && !inside(self.hits.composer_panel) {
                    self.close_composer();
                }
                if self.dialog.is_some() {
                    let Some(p) = self.hits.dialog.as_ref() else { return };
                    if !inside(p.panel) {
                        self.close_dialog();
                        return;
                    }
                    let row = p.rows.iter().find(|(ry, _)| *ry == y).copied();
                    if let Some((_, i)) = row {
                        let pick = match self.dialog.as_mut() {
                            Some(Dialog::Select(s)) => {
                                s.selected = i;
                                s.current().map(|o| (s.kind, o.value.clone()))
                            }
                            _ => None,
                        };
                        if let Some((kind, v)) = pick {
                            self.dialog_select(kind, v);
                        }
                    }
                    return;
                }
                if let Some((_, i)) = self.hits.auto.iter().find(|(ry, _)| *ry == y).copied() {
                    if let Some(opt) = self.auto.as_ref().and_then(|a| a.options.get(i).cloned()) {
                        self.pick_auto(opt, false);
                    }
                    return;
                }
                if let Some((_, i)) = self.hits.perm_options.iter().find(|(r, _)| inside(*r)).copied() {
                    self.perm.selected = i;
                    let k = Key { ctrl: false, alt: false, shift: false, name: "return".into() };
                    self.permission_key(&k, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    return;
                }
                if let Some((_, _, id)) = self.hits.tab_close.iter().find(|(a, b, _)| x >= *a && x < *b && y == 0).cloned() {
                    // The close mark only exists while the tab is hovered, so a click at that
                    // cell without a preceding motion must select the tab rather than close it.
                    if self.tab_hover.as_deref() == Some(id.as_str()) {
                        self.close_tab_id(&id);
                        return;
                    }
                }
                if let Some((_, _, id)) = self.hits.tabs.iter().find(|(a, b, _)| x >= *a && x < *b && y == 0).cloned() {
                    if id == "+" {
                        self.new_session();
                    } else if id != self.sid() || self.route != Route::Session {
                        self.open_session(&id);
                    }
                    return;
                }
                if let Some(id) = self.tab_at(x, y) {
                    if id == "+" {
                        self.new_session();
                    } else if id != self.sid() || self.route != Route::Session {
                        self.open_session(&id);
                    }
                    return;
                }
                if self.hits.jump.is_some_and(inside) {
                    self.scroll = 0;
                    return;
                }
                if inside(self.hits.transcript) {
                    let line = self.hits.transcript_skip + (y - self.hits.transcript.y) as usize;
                    if let Some((_, _, data, mime)) = self.hits.images.iter().find(|(s, e, _, _)| line >= *s && line < *e).cloned() {
                        self.dialog = Some(Dialog::Image { data, mime, scroll: 0 });
                        return;
                    }
                    if let Some((_, _, id)) = self.hits.clicks.iter().find(|(s, e, _)| line >= *s && line < *e).cloned() {
                        if let Some(sid) = id.strip_prefix("session:") {
                            let sid = sid.to_string();
                            self.open_session(&sid);
                        } else if !self.expanded.remove(&id) {
                            self.expanded.insert(id);
                        }
                        self.cache = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, s: &str) {
        let text = s.replace("\r\n", "\n").replace('\r', "\n");
        let paste_app = self.app.clone();
        match self.dialog.as_mut() {
            Some(Dialog::Select(sel)) => {
                sel.filter.insert(text.lines().next().unwrap_or(""));
                sel.set_filter();
                return;
            }
            Some(Dialog::Rename(p)) => {
                p.editor.insert(text.lines().next().unwrap_or(""));
                return;
            }
            Some(Dialog::Terminal(term)) => {
                let _ = term.write(&text);
                return;
            }
            Some(Dialog::McpAuth(a)) => {
                a.editor.insert(text.lines().next().unwrap_or(""));
                return;
            }
            Some(Dialog::Plugin { page, layout, focus, .. }) => {
                if let Some(control) = plugin_ui::controls(layout).get(*focus) {
                    if control.kind == plugin_ui::ControlKind::Input {
                        let mut value = control.value.clone();
                        value.push_str(&text);
                        plugin_ui::set_control_value(layout, *focus, value);
                    }
                }
                if !page.is_empty() {
                    let text = text.clone();
                    let app = paste_app.clone();
                    tokio::spawn(async move {
                        let _ = app.plugins.tui_paste(&text).await;
                        app.emit(AppEvent::PluginInvalidate);
                    });
                }
                return;
            }
            Some(_) => return,
            None => {}
        }
        if let Some(e) = self.perm.rejecting.as_mut() {
            e.insert(&text);
            return;
        }
        if !self.permissions.is_empty() {
            return;
        }
        if let Some(form) = self.pending_form() {
            self.form.sync(&form);
            if let Some(editor) = self.form.editor() {
                editor.insert(text.lines().next().unwrap_or(""));
            }
            return;
        }
        match prompt::paste_summary(&text) {
            Some(label) if self.settings.str("prompt.paste") != Some("full") => {
                self.editor.insert(&format!("{label} "));
                self.pasted.push((label, text.trim().to_string()));
            }
            _ => self.editor.insert(&text),
        }
        self.update_auto();
    }

    fn on_app_event(&mut self, ev: AppEvent) {
        let sid = self.sid();
        match ev {
            AppEvent::Permission(req) if req.session_id == sid || self.is_child(&req.session_id) => {
                if self.permissions.is_empty() {
                    self.perm = PermUi::default();
                }
                self.permissions.push_back(req);
            }
            AppEvent::PermissionResolved { id, .. } => self.permissions.retain(|r| r.id != id),
            AppEvent::Error { session, message } if session == sid => self.toast(Variant::Error, None, message),
            AppEvent::Busy { session, busy: false } if session == sid => self.interrupt = None,
            AppEvent::Notice { message } if !message.is_empty() && !message.starts_with("Compacting") => self.info(message),
            AppEvent::PluginPage { page, title, text, layout } => {
                let (focus, scroll) = match &self.dialog {
                    Some(Dialog::Plugin { page: current, focus, scroll, .. }) if current == &page => (*focus, *scroll),
                    _ => (0, 0),
                };
                self.dialog = Some(if layout.is_object() {
                    Dialog::Plugin { page, title, text, layout, scroll, focus }
                } else {
                    Dialog::Text { title, text, layout, scroll }
                });
            }
            AppEvent::DiffLoaded { source, files } => {
                if let Some(Dialog::Diff(view)) = self.dialog.as_mut() {
                    if view.source.name() == source {
                        view.set_files(files);
                    }
                }
            }
            AppEvent::PluginInvalidate => {
                self.plugin_slots_at = Instant::now() - Duration::from_secs(3);
                if let Some(Dialog::Plugin { page, .. }) = &self.dialog {
                    if !page.is_empty() {
                        let page = page.clone();
                        let app = self.app.clone();
                        tokio::spawn(async move {
                            if let Ok(value) = app.plugins.execute_tui_page(&page, Value::Null).await {
                                let title = value.get("title").and_then(Value::as_str).unwrap_or(&page).to_string();
                                let text = value.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                                let layout = value.get("layout").cloned().unwrap_or(Value::Null);
                                app.emit(AppEvent::PluginPage { page, title, text, layout });
                            }
                        });
                    }
                }
            }
            AppEvent::McpAuth { name, url, instructions, handle } => {
                self.dialog = Some(Dialog::McpAuth(dialog::McpAuth {
                    name,
                    url,
                    instructions,
                    editor: Editor::default(),
                    error: None,
                    handle,
                }));
            }
            AppEvent::McpAuthDone { name, error } => {
                let was_open = matches!(&self.dialog, Some(Dialog::McpAuth(auth)) if auth.name == name)
                    || matches!(&self.dialog, Some(Dialog::Text { title, .. }) if title == &name);
                if was_open {
                    self.dialog = None;
                }
                match error {
                    Some(error) if error.contains("cancelled") => {}
                    Some(error) => self.toast(Variant::Error, None, format!("MCP {name}: {error}")),
                    None => {
                        self.toast(Variant::Success, None, format!("MCP server {name} connected."));
                        if was_open {
                            self.open_mcp();
                        }
                    }
                }
            }
            AppEvent::Toast { title, message, variant } => {
                let variant = match variant.as_str() {
                    "success" => Variant::Success,
                    "warning" => Variant::Warning,
                    "error" => Variant::Error,
                    _ => Variant::Info,
                };
                self.toast(variant, title, message);
            }
            AppEvent::Updated { session } => {
                if session == sid {
                    self.tab_unread.remove(&session);
                    if !self.shells.is_empty() {
                        let done = self.handle.with(|s| s.messages.last().is_some_and(|m| m.text().starts_with("<shell-run")));
                        if done {
                            self.shells.remove(0);
                        }
                    }
                } else if self.tabs.contains(&session) {
                    self.tab_unread.insert(session);
                }
            }
            _ => {}
        }
        self.cache = None;
    }

    fn is_child(&self, session: &str) -> bool {
        self.app.session(session).map(|h| h.with(|s| s.parent_id.clone())).ok().flatten().is_some()
    }

    // ---------------------------------------------------------------- drawing

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.size = (area.width, area.height);
        let t = self.theme.clone();
        let buf = f.buffer_mut();
        draw::fill(buf, area, t.bg);
        self.hits = Hits::default();
        let mut body = area;
        let vertical = !self.mini && self.tabs_vertical() && self.route == Route::Session && !self.tabs.is_empty();
        if vertical {
            self.draw_tabs(buf, area, &t);
            body = Rect { x: area.x + 42, width: area.width.saturating_sub(42), ..area };
        } else if self.tabs_visible() && (!self.tabs.is_empty() || self.route == Route::Home) {
            self.draw_tabs(buf, Rect { height: 1, ..area }, &t);
            body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
        }
        let mut cursor = match self.route {
            Route::Home => self.draw_home(buf, body, &t),
            Route::Session => self.draw_session(buf, body, &t),
        };
        let pending = self.pending_form();
        match &pending {
            Some(form) => self.form.sync(form),
            None => {
                if !self.form.id.is_empty() {
                    self.form = forms::FormUi::default();
                }
            }
        }
        if self.dialog.is_none() && self.permissions.is_empty() {
            if let Some(form) = &pending {
                if let Some(position) = self.form.draw(buf, area, &t, form) {
                    cursor = Some(position);
                }
            }
        }
        self.draw_toast(buf, area, &t);
        if let Some(d) = self.dialog.as_mut() {
            let d_t = &t.dialog;
            match d {
                Dialog::Select(s) => {
                    let (placed, c) = dialog::draw_select(buf, area, &t, s);
                    self.hits.dialog = Some(placed);
                    cursor = Some(c);
                }
                Dialog::Diff(view) => {
                    view.draw(buf, area, &t);
                    self.hits.dialog = Some(dialog::Placed { panel: area, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Image { data, mime, scroll } => {
                    let area = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(3) };
                    draw::fill(buf, area, t.bg);
                    let lines = image_view::lines(data, mime, area.width as usize, area.height as usize, t.bg);
                    let skip = (*scroll).min(lines.len().saturating_sub(area.height as usize));
                    draw::draw_lines(buf, area, &lines, skip);
                    self.hits.dialog = Some(dialog::Placed { panel: area, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Help => {
                    let text = format!("Press {} to see all available actions and commands in any context.", self.keys.label("command.palette.show").unwrap_or_default());
                    let wrapped = draw::paragraph(&[span(text, d_t.muted, d_t.bg)], 54, draw::Wrap::Word, d_t.bg);
                    let panel = dialog::draw_text(buf, area, &t, "Help", "esc/enter", wrapped, true);
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Status => {
                    let servers = self.app.mcp.snapshot();
                    let mut lines = Vec::new();
                    if servers.is_empty() {
                        lines.push(line(vec![span("No MCP servers", d_t.text, d_t.bg)], d_t.bg));
                    } else {
                        lines.push(line(vec![span(format!("{} MCP server{}", servers.len(), if servers.len() == 1 { "" } else { "s" }), d_t.text, d_t.bg)], d_t.bg));
                        for (n, st) in servers {
                            let (c, s) = match st {
                                Some(crate::mcp::Status::Connected { .. }) => (d_t.success.base, "Connected".to_string()),
                                Some(crate::mcp::Status::Failed(e)) => (d_t.error.base, e),
                                Some(crate::mcp::Status::NeedsAuth(e)) => (d_t.warning.base, format!("Sign in required: {e}")),
                                Some(crate::mcp::Status::Disabled) => (d_t.muted, "Disabled in configuration".into()),
                                None => (d_t.muted, "pending".into()),
                            };
                            lines.push(line(vec![span("• ", c, d_t.bg), Span::styled(n, draw::bold(draw::st(d_t.text, d_t.bg))), span(format!(" {s}"), d_t.muted, d_t.bg)], d_t.bg));
                        }
                    }
                    let panel = dialog::draw_text(buf, area, &t, "Status", "esc", lines, false);
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Terminal(term) => {
                    term.refresh();
                    let lines: Vec<Line<'static>> = term
                        .lines()
                        .iter()
                        .map(|line| Line::from(span(line.clone(), d_t.text, d_t.bg)))
                        .collect();
                    let title = if term.title().is_empty() { "Terminal" } else { term.title() };
                    let (panel, position) =
                        dialog::draw_terminal(buf, area, &t, title, "esc", lines, term.cursor());
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = position;
                }
                Dialog::McpAuth(auth) => {
                    let (panel, position) = dialog::draw_mcp_auth(buf, area, &t, auth);
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = Some(position);
                }
                Dialog::Text { title, text, scroll, layout } => {
                    let mut lines = plugin_ui::render(layout, area.width.saturating_sub(6).max(20) as usize, &t, d_t.bg);
                    if lines.is_empty() {
                        for paragraph in text.split('\n') {
                            if paragraph.is_empty() {
                                lines.push(line(vec![], d_t.bg));
                            } else {
                                lines.extend(draw::paragraph(
                                    &[span(paragraph.to_string(), d_t.text, d_t.bg)],
                                    60,
                                    draw::Wrap::Word,
                                    d_t.bg,
                                ));
                            }
                        }
                    }
                    let visible: Vec<Line<'static>> = lines.into_iter().skip(*scroll).collect();
                    let panel = dialog::draw_text(buf, area, &t, title, "esc", visible, false);
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Plugin { title, text, scroll, layout, focus, .. } => {
                    let mut lines = plugin_ui::render_focused(
                        layout,
                        area.width.saturating_sub(6).max(20) as usize,
                        &t,
                        d_t.bg,
                        *focus,
                    );
                    if lines.is_empty() {
                        for paragraph in text.split('\n') {
                            if paragraph.is_empty() {
                                lines.push(line(vec![], d_t.bg));
                            } else {
                                lines.extend(draw::paragraph(
                                    &[span(paragraph.to_string(), d_t.text, d_t.bg)],
                                    60,
                                    draw::Wrap::Word,
                                    d_t.bg,
                                ));
                            }
                        }
                    }
                    let visible: Vec<Line<'static>> = lines.into_iter().skip(*scroll).collect();
                    let panel = dialog::draw_text(buf, area, &t, title, "esc", visible, false);
                    self.hits.dialog = Some(dialog::Placed { panel, list: Rect::default(), rows: vec![] });
                    cursor = None;
                }
                Dialog::Rename(p) => {
                    cursor = Some(dialog::draw_prompt(buf, area, &t, p));
                    self.hits.dialog = Some(dialog::Placed { panel: Rect { x: 0, y: 0, width: area.width, height: area.height }, list: Rect::default(), rows: vec![] });
                }
            }
        }
        if let Some((x, y)) = cursor {
            f.set_cursor_position((x, y));
        }
    }

        fn tab_layout(&self, avail: usize, current: Option<&str>) -> (usize, usize, usize, usize, usize) {
        let n = self.tabs.len();
        if n == 0 {
            return (0, 0, 0, 0, 32);
        }
        let active = current.and_then(|id| self.tabs.iter().position(|t| t == id)).unwrap_or(0);
        let min = 12usize;
        let max = 32usize;
        if n * max <= avail {
            return (0, n, 0, 0, max);
        }
        if n * min <= avail {
            return (0, n, 0, 0, (avail / n).min(max));
        }
        let mut count = (avail / min).max(1).min(n);
        let mut start = active.saturating_sub(count / 2);
        if start + count > n {
            start = n - count;
        }
        let before = start;
        let after = n - start - count;
        let reserved = (if before > 0 { 4 } else { 0 }) + (if after > 0 { 4 } else { 0 });
        let mut width = ((avail.saturating_sub(reserved)) / count).max(8);
        if width < 8 {
            count = 1;
            width = 8;
        }
        (start, count, before, after, width)
    }

    fn draw_tab(&mut self, buf: &mut Buffer, x: u16, y: u16, w: u16, id: &str, index: usize, t: &Theme, cur: Option<&str>) {
        let Ok(h) = self.app.session(id) else { return };
        let title = h.with(|s| if s.has_default_title() { "New session".to_string() } else { s.title.clone() });
        let active = cur == Some(id);
        let hovered = self.tab_hover.as_deref() == Some(id);
        let bg = if hovered {
            if active { t.raised_high } else { t.raised }
        } else if active {
            t.raised_high
        } else {
            t.bg
        };
        let numbers = self.settings.tabs_indicators() == "numbers";
        let unread = self.tab_unread.contains(id);
        let busy = h.is_busy();
        let ts = if active {
            draw::bold(draw::st(t.text, bg))
        } else if hovered {
            draw::st(t.text, bg)
        } else {
            draw::st(t.muted, bg)
        };
        let xs = draw::st(t.muted, bg);
        let prefix_w = if numbers { 4 } else { 3 };
        let title_width = (w as usize).saturating_sub(prefix_w + if hovered { 2 } else { 0 }).max(1);
        let mut label = if hovered {
            tab_marquee(&title, title_width, self.tab_hover_at.map(|at| at.elapsed()))
        } else {
            draw::take_width(&title, title_width)
        };
        let used = draw::width(&label);
        if used < title_width {
            label.push_str(&" ".repeat(title_width - used));
        }
        let mut spans = Vec::new();
        if numbers {
            spans.push(span(" ", t.text, bg));
            let color = if unread { t.hue("accent", 200) } else if active || hovered { t.text } else { t.muted };
            spans.push(span(format!("{:>2}", index + 1), color, bg));
            spans.push(span(" ", t.text, bg));
        } else {
            spans.push(span(" ", t.text, bg));
            let (mark, color) = if busy {
                (SPINNER[(self.frame / 2) % SPINNER.len()], t.hue("interactive", 200))
            } else if unread {
                ("\u{2022}", t.hue("accent", 200))
            } else {
                (" ", t.muted)
            };
            spans.push(span(mark, color, bg));
            spans.push(span(" ", t.text, bg));
        }
        spans.push(Span::styled(label, ts));
        if hovered {
            spans.push(Span::styled("\u{2715}", xs));
            spans.push(span(" ", t.text, bg));
        }
        draw::draw_line(buf, x, y, w, &line(spans, bg));
        self.hits.tabs.push((x, x + w, id.to_string()));
        self.hits.tab_close.push((x + w - 2, x + w - 1, id.to_string()));
    }

    fn draw_tabs(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        if !self.tabs_visible() {
            return;
        }
        if self.tabs_vertical() && self.route == Route::Session {
            self.draw_tabs_vertical(buf, area, t);
            return;
        }
        let cur = if self.route == Route::Session { Some(self.sid()) } else { None };
        let n = self.tabs.len();
        let mut x = area.x;
        if n > 0 {
            let plus_w = if self.route == Route::Home { 0 } else { 3 };
            let avail = (area.width as usize).saturating_sub(plus_w);
            let (start, count, before, after, w) = self.tab_layout(avail, cur.as_deref());
            if before > 0 {
                let label = draw::take_width(&format!("\u{2039}{before} "), 4);
                draw::draw_line(buf, x, area.y, 4, &line(vec![span(format!("{label:<4}"), t.muted, t.bg)], t.bg));
                x += 4;
            }
            for index in start..start + count {
                let id = self.tabs[index].clone();
                self.draw_tab(buf, x, area.y, w as u16, &id, index, t, cur.as_deref());
                x += w as u16;
            }
            if after > 0 {
                let label = draw::take_width(&format!(" {after}\u{203a}"), 4);
                draw::draw_line(buf, x, area.y, 4, &line(vec![span(format!("{label:<4}"), t.muted, t.bg)], t.bg));
                x += 4;
            }
        }
        if self.route == Route::Home {
            let w = 32u16.min((area.x + area.width).saturating_sub(x));
            if w >= 6 {
                let bg = t.raised_high;
                let plus = draw::bold(draw::st(theme::tint(t.text, bg, 0.25), bg));
                let spans = vec![span(" ", t.text, bg), Span::styled("+", plus), span(" ", t.text, bg), Span::styled(draw::truncate("New session", w as usize - 4), draw::bold(draw::st(t.text, bg)))];
                draw::draw_line(buf, x, area.y, w, &line(spans, bg));
                self.hits.tabs.push((x, x + w, "+".into()));
            }
        } else if x + 3 <= area.x + area.width {
            draw::draw_line(buf, x, area.y, 3, &line(vec![span(" + ", t.muted, t.bg)], t.bg));
            self.hits.tabs.push((x, x + 3, "+".into()));
        }
    }

    fn draw_tabs_vertical(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let rail_w = 42u16.min(area.width);
        let bg = t.raised;
        draw::fill(buf, Rect { x: area.x, y: area.y, width: rail_w, height: area.height }, bg);
        let cur = Some(self.sid());
        let mut y = area.y;
        for (index, id) in self.tabs.clone().into_iter().enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let Ok(h) = self.app.session(&id) else { continue };
            let title = h.with(|s| if s.has_default_title() { "New session".to_string() } else { s.title.clone() });
            let active = cur.as_deref() == Some(id.as_str());
            let hovered = self.tab_hover.as_deref() == Some(id.as_str());
            let row_bg = if hovered { t.raised_high } else if active { t.raised_high } else { bg };
            let numbers = self.settings.tabs_indicators() == "numbers";
            let unread = self.tab_unread.contains(&id);
            let busy = h.is_busy();
            let mark = if numbers {
                format!("{:>2}", index + 1)
            } else if busy {
                SPINNER[(self.frame / 2) % SPINNER.len()].to_string()
            } else if unread {
                "\u{2022}".to_string()
            } else {
                " ".to_string()
            };
            let mark_color = if unread { t.hue("accent", 200) } else if active || hovered { t.text } else { t.muted };
            let title_width = (rail_w as usize).saturating_sub(7);
            let label = if hovered {
                tab_marquee(&title, title_width, self.tab_hover_at.map(|at| at.elapsed()))
            } else {
                draw::take_width(&title, title_width)
            };
            let label = format!("{label:<width$}", width = title_width);
            let ts = if active || hovered { draw::st(t.text, row_bg) } else { draw::st(t.muted, row_bg) };
            let spans = vec![
                span(" ", t.text, row_bg),
                span(mark, mark_color, row_bg),
                span(" ", t.text, row_bg),
                Span::styled(label, ts),
            ];
            draw::draw_line(buf, area.x + 1, y, rail_w.saturating_sub(2), &line(spans, row_bg));
            self.hits.tabs_vertical.push((Rect { x: area.x, y, width: rail_w, height: 1 }, id));
            y += 1;
        }
    }

    fn draw_home(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) -> Option<(u16, u16)> {
        let (w, h) = (area.width as usize, area.height);
        let footer_h: u16 = if self.mini && !self.settings.mini_footer() {
            0
        } else if h >= 14 && w >= 44 {
            if h < 16 {
                1
            } else {
                3
            }
        } else {
            0
        };
        let main_h = area.height.saturating_sub(footer_h);
        let pad = if w < 44 { 1 } else { 2 };
        let prompt_w = 75.min(w.saturating_sub(pad * 2)) as u16;
        let prompt_h = self.prompt_height(prompt_w as usize) as u16;
        let logo_h: u16 = if (self.mini && !self.settings.mini_splash()) || h < 12 || w < 44 { 0 } else { 4 };
        let fixed = 3 + logo_h + 1 + 1 + prompt_h;
        let free = main_h.saturating_sub(fixed);
        let spacer = if main_h >= fixed { 3 } else { 3u16.saturating_sub(fixed - main_h) };
        let mut y = area.y + free / 2 + spacer;
        if logo_h > 0 {
            let render = |s: &str, fg: Rgba, bold: bool| -> Vec<Span<'static>> {
                let shadow = theme::tint(t.bg, fg, 0.25);
                s.chars()
                    .map(|c| {
                        let (ch, mut st) = match c {
                            '_' => (' ', draw::st(fg, shadow)),
                            '^' => ('▀', draw::st(fg, shadow)),
                            '~' => ('▀', draw::st(shadow, t.bg)),
                            ',' => ('▄', draw::st(shadow, t.bg)),
                            o => (o, draw::st(fg, t.bg)),
                        };
                        if bold {
                            st = draw::bold(st);
                        }
                        Span::styled(ch.to_string(), st)
                    })
                    .collect()
            };
            let x = area.x + pad as u16 + (((w - pad * 2) as f32 - 39.0) / 2.0).round().max(0.0) as u16;
            for i in 0..4 {
                let mut l = render(LOGO_LEFT[i], t.muted, false);
                l.push(span(" ", t.text, t.bg));
                l.extend(render(LOGO_RIGHT[i], t.text, true));
                draw::draw_line(buf, x, y + i as u16, 39, &line(l, t.bg));
            }
            y += 4;
        }
        y += 2; // spacer and the prompt's top padding
        let x = area.x + pad as u16 + (((w - pad * 2) as f32 - prompt_w as f32) / 2.0).round().max(0.0) as u16;
        let cursor = self.draw_prompt(buf, Rect { x, y, width: prompt_w, height: prompt_h }, t, true);
        if footer_h > 0 {
            let fy = area.y + area.height - footer_h + if footer_h == 3 { 1 } else { 0 };
            let servers = self.app.mcp.snapshot();
            let mut spans = vec![span("  ", t.text, t.bg)];
            if !servers.is_empty() {
                let failed = servers
                    .iter()
                    .filter(|(_, s)| matches!(s, Some(crate::mcp::Status::Failed(_)) | Some(crate::mcp::Status::NeedsAuth(_))))
                    .count();
                let ok = servers.iter().filter(|(_, s)| matches!(s, Some(crate::mcp::Status::Connected { .. }))).count();
                if failed > 0 {
                    spans.push(span("⊙ ", t.error.base, t.bg));
                    spans.push(span(format!("{failed} MCP needs attention"), t.text, t.bg));
                } else {
                    spans.push(span("⊙ ", if ok > 0 { t.success.base } else { t.muted }, t.bg));
                    spans.push(span(format!("{ok} MCP"), t.text, t.bg));
                }
                if w >= 64 {
                    spans.push(span(" /mcps", t.muted, t.bg));
                }
            }
            if w >= 64 {
                let used: usize = spans.iter().map(|s| draw::width(&s.content)).sum();
                let v = crate::VERSION;
                spans.push(span(" ".repeat(w.saturating_sub(used + v.len() + 2)), t.text, t.bg));
                spans.push(span(v, t.muted, t.bg));
            }
            draw::draw_line(buf, area.x, fy, area.width, &line(spans, t.bg));
        }
        cursor
    }

    /// Composer height for an outer width: padding, text rows, metadata, bottom edge, footer.
    fn prompt_height(&self, outer: usize) -> usize {
        let pad = if self.size.0 < 44 { 1 } else { 2 };
        let text_w = outer.saturating_sub(1 + pad * 2).max(1);
        let max = (self.size.1 as usize / 3).max(6);
        let (rows, _) = self.editor.layout(text_w);
        let n = if self.editor.text.is_empty() { 1 } else { rows.len().clamp(1, max) };
        1 + n + 2 + 1 + 1
    }

    fn draw_prompt(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, home: bool) -> Option<(u16, u16)> {
        let pad = if self.size.0 < 44 { 1u16 } else { 2 };
        let muted = self.leader.is_some_and(|l| l.elapsed() < LEADER_TIMEOUT);
        let highlight = if muted {
            t.border
        } else if self.shell {
            t.text_primary.selected
        } else {
            self.agent_color(&self.agent)
        };
        let border = theme::tint(t.border, highlight, 1.0);
        let bg = t.decrease(t.raised);
        let text_w = area.width.saturating_sub(1 + pad * 2).max(1) as usize;
        let max = (self.size.1 as usize / 3).max(6);
        let (rows, (cr, cc)) = self.editor.layout(text_w);
        let n = if self.editor.text.is_empty() { 1 } else { rows.len().clamp(1, max) };
        let first = cr.saturating_sub(n - 1);
        let stripe = span("┃", border, t.bg);
        let row = |spans: Vec<Span<'static>>| {
            let mut s = vec![stripe.clone(), span(" ".repeat(pad as usize), t.text, bg)];
            s.extend(spans);
            line(s, bg)
        };
        let mut lines = vec![row(vec![])];
        if self.editor.text.is_empty() {
            if home || self.shell {
                lines.push(row(vec![span(prompt::placeholder(self.shell, self.placeholder, self.size.0), t.muted, bg)]));
            } else {
                lines.push(row(vec![]));
            }
        } else {
            let fg = if muted { t.muted } else { t.text };
            for (_, r) in rows.iter().skip(first).take(n) {
                // pasted summaries are highlighted like upstream's extmarks
                let mut spans = Vec::new();
                let mut rest = r.as_str();
                while !rest.is_empty() {
                    match self.pasted.iter().filter_map(|(p, _)| rest.find(p.as_str()).map(|i| (i, p.len()))).min() {
                        Some((i, len)) => {
                            if i > 0 {
                                spans.push(span(rest[..i].to_string(), fg, bg));
                            }
                            spans.push(span(rest[i..i + len].to_string(), t.warning.base, bg));
                            rest = &rest[i + len..];
                        }
                        None => {
                            spans.push(span(rest.to_string(), fg, bg));
                            rest = "";
                        }
                    }
                }
                lines.push(row(spans));
            }
        }
        lines.push(row(vec![]));
        lines.push(row(self.metadata_spans(t, bg, highlight, muted)));
        for (i, l) in lines.iter().enumerate() {
            draw::draw_line(buf, area.x, area.y + i as u16, area.width, l);
        }
        let mut y = area.y + lines.len() as u16;
        let edge = line(vec![span("╹", border, t.bg), span("▀".repeat(area.width.saturating_sub(1) as usize), bg, t.bg)], t.bg);
        draw::draw_line(buf, area.x, y, area.width, &edge);
        y += 1;
        self.draw_prompt_footer(buf, Rect { x: area.x, y, width: area.width, height: 1 }, t, home);
        let cursor = (area.x + 1 + pad + cc as u16, area.y + 1 + (cr - first) as u16);
        if let Some(a) = self.auto.as_ref() {
            let h = a.options.len().clamp(1, 10).min(area.y as usize).max(1) as u16;
            let top = area.y.saturating_sub(h);
            let pbg = t.raised_high;
            let start = a.selected.saturating_sub(h as usize - 1);
            let inner = area.width.saturating_sub(2) as usize;
            let right = |buf: &mut Buffer, y: u16| draw::draw_line(buf, area.x + area.width - 1, y, 1, &line(vec![span("┃", t.border, t.bg)], t.bg));
            if a.options.is_empty() {
                let msg = if a.kind == AutoKind::Command { "No matching items" } else { "No matching files, agents, or references" };
                draw::draw_line(buf, area.x, top, area.width - 1, &line(vec![span("┃", t.border, t.bg), span(format!(" {msg}"), t.muted, pbg)], pbg));
                right(buf, top);
            }
            for (i, o) in a.options.iter().enumerate().skip(start).take(h as usize) {
                let ry = top + (i - start) as u16;
                let sel = i == a.selected;
                let rbg = if sel { t.bg_primary.focused } else { pbg };
                let fg = if sel { t.text_primary.focused } else { t.text };
                let sub = if sel { t.text_primary.focused } else { t.muted };
                let label_w = if o.label.is_empty() { 0 } else { o.label.len() + 2 };
                let content = inner.saturating_sub(2 + label_w);
                let disp = draw::truncate_middle(&o.display, content);
                let mut used = draw::width(&disp);
                let mut spans = vec![span("┃", t.border, t.bg), span(" ", fg, rbg), span(disp, fg, rbg)];
                if !o.description.is_empty() && used < content {
                    let d = draw::take_width(&format!(" {}", o.description.split_whitespace().collect::<Vec<_>>().join(" ")), content - used);
                    used += draw::width(&d);
                    spans.push(span(d, sub, rbg));
                }
                if label_w > 0 {
                    spans.push(span(" ".repeat(content.saturating_sub(used) + 2), fg, rbg));
                    spans.push(span(o.label, sub, rbg));
                }
                draw::draw_line(buf, area.x, ry, area.width - 1, &line(spans, rbg));
                right(buf, ry);
                self.hits.auto.push((ry, i));
            }
        }
        if self.dialog.is_none() && self.permissions.is_empty() {
            Some(cursor)
        } else {
            None
        }
    }

    /// `PromptMetadataRow` with upstream's fitting candidates.
    fn metadata_spans(&self, t: &Theme, bg: Rgba, highlight: Rgba, muted: bool) -> Vec<Span<'static>> {
        if self.shell {
            return vec![span("Shell", highlight, bg)];
        }
        let w = self.size.0 as usize;
        let agent = if w < 44 { None } else { Some(titlecase(&self.agent)) };
        let (model, provider) = self.model_labels();
        let provider = if w < 44 { String::new() } else { provider };
        let variant = self.variant.clone();
        let avail = w.saturating_sub(if w < 44 { 9 } else { 13 });
        let text = |m: &str, p: &str| {
            let mut s = agent.clone().map(|a| format!("{a} · ")).unwrap_or_default();
            s.push_str(m);
            if !p.is_empty() {
                s.push(' ');
                s.push_str(p);
            }
            if let Some(v) = &variant {
                s.push_str(&format!(" · {v}"));
            }
            s
        };
        let short = provider.rsplit(" / ").next().unwrap_or(&provider).to_string();
        let (model, provider) = if draw::width(&text(&model, &provider)) <= avail {
            (model, provider)
        } else if draw::width(&text(&model, &short)) <= avail {
            (model, short)
        } else if draw::width(&text(&model, "")) <= avail {
            (model, String::new())
        } else {
            let prefix = agent.clone().map(|a| format!("{a} · ")).unwrap_or_default();
            let suffix = variant.as_ref().map(|v| format!(" · {v}")).unwrap_or_default();
            (draw::truncate(&model, avail.saturating_sub(draw::width(&prefix) + draw::width(&suffix)).max(9)), String::new())
        };
        let mut spans = Vec::new();
        if let Some(a) = agent {
            spans.push(span(a, highlight, bg));
            spans.push(span(" · ", t.muted, bg));
        }
        spans.push(span(model, if muted { t.muted } else { t.text }, bg));
        if !provider.is_empty() {
            spans.push(span(format!(" {provider}"), t.muted, bg));
        }
        if let Some(v) = variant {
            spans.push(span(" · ", t.muted, bg));
            spans.push(Span::styled(v, draw::bold(draw::st(t.warning.base, bg))));
        }
        spans
    }

    /// Context tokens of the last step and the session cost.
    fn usage(&self) -> Vec<String> {
        let children = self.app.active_children(&self.sid());
        self.handle.with(|s| {
            let mut out = Vec::new();
            if children > 0 {
                out.push(format!("{children} subagent{}", if children == 1 { "" } else { "s" }));
            }
            if let Some(m) = s.messages.iter().rev().find(|m| m.role == Role::Assistant && (m.usage.input + m.usage.output + m.usage.cache_read) > 0) {
                let u = &m.usage;
                let tokens = u.input + u.output + u.reasoning + u.cache_read + u.cache_write;
                let ctx = m.model.as_deref().and_then(|full| crate::models::resolve(&self.app.providers.read().unwrap(), full).ok()).map(|r| r.model.context).unwrap_or(0);
                if ctx > 0 {
                    out.push(format!("{} ({}%)", transcript::number(tokens), ((tokens as f64 / ctx as f64) * 100.0).round() as u64));
                } else {
                    out.push(transcript::number(tokens));
                }
            }
            let (_, cost) = s.usage_total();
            if cost > 0.0 {
                out.push(format!("${cost:.2}"));
            }
            out
        })
    }

    fn cwd(&self) -> std::path::PathBuf {
        if self.route == Route::Session {
            self.handle.with(|session| session.directory.clone())
        } else {
            self.app.directory.clone()
        }
    }

    fn location_label(&self) -> String {
        let home = crate::paths::get().home.to_string_lossy().to_string();
        let directory = self.cwd();
        let dir = draw::abbreviate_home(&directory.to_string_lossy(), &home);
        match git_branch(&directory) {
            Some(b) => format!("{dir}:{b}"),
            None => dir,
        }
    }

    fn draw_prompt_footer(&self, buf: &mut Buffer, area: Rect, t: &Theme, home: bool) {
        let bg = t.bg;
        let w = area.width as usize;
        let busy = !home && self.handle.is_busy();
        let cmd = self.keys.label("command.palette.show").unwrap_or_default();
        let usage = if home { vec![] } else { self.usage() };
        let term_w = self.size.0 as usize;
        let show_details = self.interrupt.is_none() || term_w >= 80;
        let mut right: Vec<Span<'static>> = Vec::new();
        if self.shell {
            right.push(span("esc ", t.text, bg));
            right.push(span(if term_w < 44 { "shell" } else { "exit shell mode" }, t.muted, bg));
        } else if !usage.is_empty() {
            let width = term_w.saturating_sub(8);
            let avail = width.saturating_sub(28.min(width / 2));
            let u = usage.join(" · ");
            if show_details && draw::width(&format!("{u} · {cmd} commands")) <= avail {
                right.extend([span(u, t.muted, bg), span("  ", t.muted, bg), span(format!("{cmd} "), t.text, bg), span("commands", t.muted, bg)]);
            } else if show_details && draw::width(&u) <= avail {
                right.push(span(u, t.muted, bg));
            }
        } else if show_details && term_w >= 44 {
            right.extend([
                span(format!("{} ", self.keys.label("agent.cycle").unwrap_or_default()), t.text, bg),
                span("agents", t.muted, bg),
                span("  ", t.muted, bg),
                span(format!("{cmd} "), t.text, bg),
                span("commands", t.muted, bg),
            ]);
        }
        let rw: usize = right.iter().map(|s| draw::width(&s.content)).sum();
        let left_w = w.saturating_sub(rw + if rw > 0 { 2 } else { 0 });
        let mut spans: Vec<Span<'static>> = Vec::new();
        if busy {
            spans.push(span(" ", t.text, bg));
            spans.extend(self.scanner(t, bg));
            spans.push(span(" ", t.text, bg));
            if self.interrupt.is_some_and(|i| i.elapsed() < Duration::from_secs(5)) {
                spans.push(span("esc again to interrupt", t.warning.base, bg));
            } else {
                spans.extend([span("esc ", t.text, bg), span("interrupt", t.muted, bg)]);
            }
        } else {
            let mut label = self.location_label();
            if let Some((_, status)) = self.goal_state() {
                label = format!("{label}  goal {status}");
            }
            if !self.plugin_status.is_empty() {
                label = format!("{label}  ·  {}", self.plugin_status);
            }
            spans.push(span(draw::truncate(&label, left_w), t.muted, bg));
        }
        let lw: usize = spans.iter().map(|s| draw::width(&s.content)).sum();
        spans.push(span(" ".repeat(w.saturating_sub(lw + rw)), t.text, bg));
        spans.extend(right);
        draw::draw_line(buf, area.x, area.y, area.width, &line(spans, bg));
    }

    /// The prompt's "blocks" scanner (`ui/spinner.ts` createFrames/createColors).
    fn scanner(&self, t: &Theme, bg: Rgba) -> Vec<Span<'static>> {
        if !self.settings.animations() {
            return vec![span("[⋯]", t.muted, bg)];
        }
        let color = self.agent_color(&self.agent);
        let width = 8usize;
        let (hold_start, hold_end) = (30usize, 9usize);
        let total = width + hold_end + (width - 1) + hold_start;
        let frame = self.frame % total;
        let trail: Vec<Rgba> = (0..6)
            .map(|i| {
                let (alpha, bright) = match i {
                    0 => (1.0, 1.0),
                    1 => (0.9, 1.15),
                    n => (0.65f32.powi(n - 1), 1.0),
                };
                Rgba { r: (color.r * bright).min(1.0), g: (color.g * bright).min(1.0), b: (color.b * bright).min(1.0), a: alpha }
            })
            .collect();
        let (pos, holding, hold_progress, hold_total, move_progress, move_total, forward) = if frame < width {
            (frame as isize, false, 0, 0, frame, width, true)
        } else if frame < width + hold_end {
            ((width - 1) as isize, true, frame - width, hold_end, 0, 0, true)
        } else if frame < width + hold_end + width - 1 {
            let b = frame - width - hold_end;
            (width as isize - 2 - b as isize, false, 0, 0, b, width - 1, false)
        } else {
            (0, true, frame - width - hold_end - (width - 1), hold_start, 0, 0, false)
        };
        let min_alpha = 0.3f32;
        let fade = if holding && hold_total > 0 {
            (1.0 - (hold_progress as f32 / hold_total as f32).min(1.0) * (1.0 - min_alpha)).max(min_alpha)
        } else if !holding && move_total > 0 {
            min_alpha + (move_progress as f32 / (move_total.max(2) - 1) as f32).min(1.0) * (1.0 - min_alpha)
        } else {
            1.0
        };
        let inactive = color.with_alpha(0.6 * fade);
        (0..width)
            .map(|ci| {
                let d = if forward { pos - ci as isize } else { ci as isize - pos };
                let idx = if holding {
                    d + hold_progress as isize
                } else if d >= 0 && d < trail.len() as isize {
                    d
                } else {
                    -1
                };
                let active = idx >= 0 && (idx as usize) < trail.len();
                span(if active { "■" } else { "⬝" }, if active { trail[idx as usize] } else { inactive }, bg)
            })
            .collect()
    }

    fn draw_session(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) -> Option<(u16, u16)> {
        let w = area.width as usize;
        let terminal_visible = !self.mini && self.terminal_pane.visible && !self.terminal_pane.tabs.is_empty();
        let terminal_w = if terminal_visible {
            self.terminal_width().min(area.width.saturating_sub(24))
        } else {
            0
        };
        let sidebar = !terminal_visible && self.sidebar_visible();
        let docked = sidebar && w > 120;
        let pane_w = area.width.saturating_sub(if terminal_visible { terminal_w } else if docked { 42 } else { 0 });
        let pad = if w < 44 { 1u16 } else { 2 };
        let pane = Rect { x: area.x + pad, y: area.y, width: pane_w.saturating_sub(pad * 2), height: area.height.saturating_sub(1) };
        let perm = self.permissions.front().cloned();
        let composer_h = match &perm {
            Some(req) => self.permission_lines(req, t, pane.width as usize).len() as u16,
            None => self.prompt_height(pane.width as usize) as u16,
        };
        let composer_panel_h = if self.composer_open { 7 } else { 0 };
        let composer_y = pane.y + pane.height.saturating_sub(composer_h);
        let composer_panel_y = composer_y.saturating_sub(composer_panel_h);
        let jump_y = composer_panel_y.saturating_sub(1);
        let tr = Rect { x: pane.x, y: pane.y, width: pane.width, height: jump_y.saturating_sub(pane.y) };
        self.body_height = tr.height as usize;
        self.render_transcript(tr.width as usize);
        let out = &self.cache.as_ref().unwrap().out;
        let total = out.lines.len();
        let h = tr.height as usize;
        self.scroll = self.scroll.min(total.saturating_sub(h));
        let skip = if total <= h { 0 } else { total - h - self.scroll };
        draw::draw_lines(buf, tr, &out.lines, skip);
        self.hits.transcript = tr;
        self.hits.transcript_skip = skip;
        self.hits.clicks = out.clicks.clone();
        self.hits.images = out.images.clone();
        if self.scroll > 0 {
            let label = "Jump to latest ↓";
            let lw = draw::width(label) as u16;
            let r = Rect { x: pane.x + pane.width.saturating_sub(lw), y: jump_y, width: lw, height: 1 };
            draw::draw_line(buf, r.x, r.y, r.width, &line(vec![span(label, t.text_secondary.base, t.bg)], t.bg));
            self.hits.jump = Some(r);
        }
        let composer = Rect { x: pane.x, y: composer_y, width: pane.width, height: composer_h };
        if self.composer_open {
            self.draw_composer(buf, Rect { x: pane.x, y: composer_panel_y, width: pane.width, height: composer_panel_h }, t);
        }
        let cursor = match perm.as_ref() {
            Some(req) => {
                self.draw_permission(buf, composer, t, req);
                None
            }
            None => self.draw_prompt(buf, composer, t, false),
        };
        let terminal_cursor = if terminal_visible {
            self.draw_terminal_pane(buf, Rect { x: area.x + area.width - terminal_w, y: area.y, width: terminal_w, height: area.height }, t)
        } else {
            None
        };
        if sidebar {
            let sw = 42u16.min(area.width);
            if !docked {
                draw::dim(buf, area, 70);
            }
            self.draw_sidebar(buf, Rect { x: area.x + area.width - sw, y: area.y, width: sw, height: area.height }, t);
        }
        if self.composer_open {
            None
        } else if terminal_visible && self.terminal_pane.focus && perm.is_none() && self.dialog.is_none() {
            terminal_cursor
        } else {
            cursor
        }
    }

    fn draw_composer(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let bg = t.raised;
        draw::fill(buf, area, bg);
        self.hits.composer_panel = area;
        let labels = ["Subagents", "Shell", "Terminals"];
        let mut x = area.x + 1;
        for (index, label) in labels.iter().enumerate() {
            let active = index == self.composer_tab;
            let width = draw::width(label) as u16 + 2;
            let row_bg = if active { t.raised_high } else { bg };
            let fg = if active { t.text } else { t.muted };
            draw::draw_line(buf, x, area.y, width, &line(vec![span(" ", fg, row_bg), Span::styled(*label, draw::st(fg, row_bg))], row_bg));
            self.hits.composer_tabs.push((Rect { x, y: area.y, width, height: 1 }, index));
            x += width + 2;
        }
        draw::draw_line(buf, area.x + area.width.saturating_sub(4), area.y, 4, &line(vec![span("esc ", t.muted, bg)], bg));
        let items = self.composer_items();
        let rows = area.height.saturating_sub(2) as usize;
        for (index, (_, label)) in items.iter().take(rows).enumerate() {
            let y = area.y + 2 + index as u16;
            let selected = index == self.composer_selected;
            let row_bg = if selected { t.raised_high } else { bg };
            let fg = if selected { t.text } else { t.muted };
            let label = draw::take_width(label, area.width.saturating_sub(4) as usize);
            draw::draw_line(buf, area.x + 1, y, area.width.saturating_sub(2), &line(vec![span(" ", fg, row_bg), Span::styled(label, draw::st(fg, row_bg))], row_bg));
            self.hits.composer_rows.push((Rect { x: area.x + 1, y, width: area.width.saturating_sub(2), height: 1 }, index));
        }
        if items.is_empty() {
            draw::draw_line(buf, area.x + 1, area.y + 2, area.width.saturating_sub(2), &line(vec![span("No entries", t.muted, bg)], bg));
        }
    }

    fn draw_terminal_pane(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) -> Option<(u16, u16)> {
        if area.width < 8 || area.height < 3 {
            return None;
        }
        let bg = t.raised;
        draw::fill(buf, area, bg);
        let inner_w = area.width.saturating_sub(2);
        let inner_h = area.height.saturating_sub(2);
        if let Some(terminal) = self.terminal_pane.active_mut() {
            let (cols, rows) = terminal.size();
            if cols != inner_w || rows != inner_h {
                let _ = terminal.resize(inner_w.max(1), inner_h.max(1));
            }
        }
        self.terminal_pane.refresh();
        self.hits.terminal_pane = area;
        let titles = self.terminal_pane.tab_titles();
        let mut x = area.x + 1;
        for (index, title) in titles.iter().enumerate() {
            if x >= area.x + area.width.saturating_sub(2) {
                break;
            }
            let active = index == self.terminal_pane.active;
            let label = draw::take_width(title, 20);
            let width = draw::width(&label) as u16 + 2;
            let row_bg = if active { t.raised_high } else { bg };
            let fg = if active { t.text } else { t.muted };
            draw::draw_line(buf, x, area.y, width.min(area.x + area.width - x), &line(vec![span(" ", fg, row_bg), Span::styled(label, draw::st(fg, row_bg))], row_bg));
            self.hits.terminal_tabs.push((Rect { x, y: area.y, width, height: 1 }, index));
            x += width + 1;
        }
        let close = Rect { x: area.x + area.width.saturating_sub(3), y: area.y, width: 2, height: 1 };
        draw::draw_line(buf, close.x, close.y, close.width, &line(vec![span("\u{2715} ", t.muted, bg)], bg));
        self.hits.terminal_close.push((close, self.terminal_pane.active));
        let lines = self.terminal_pane.lines();
        for (row, text) in lines.iter().take(inner_h as usize).enumerate() {
            let text_line = draw::take_width(text, inner_w as usize);
            draw::draw_line(buf, area.x + 1, area.y + 1 + row as u16, inner_w, &line(vec![span(text_line, t.text, bg)], bg));
        }
        if self.terminal_pane.focus {
            self.terminal_pane.cursor().map(|(cx, cy)| (area.x + 1 + cx.min(inner_w.saturating_sub(1)), area.y + 1 + cy.min(inner_h.saturating_sub(1))))
        } else {
            None
        }
    }

    fn render_transcript(&mut self, width: usize) {
        use std::hash::{Hash, Hasher};
        let busy = self.handle.is_busy();
        let queued: Vec<String> = self.handle.queue.lock().unwrap().iter().map(|(m, _)| m.text()).collect();
        let message_count = self.handle.with(|s| s.messages.len());
        let home = crate::paths::get().home.to_string_lossy().to_string();
        let base = self.cwd().to_string_lossy().to_string();
        let perm_tool = self.permissions.front().and_then(|p| p.tool_call_id.clone());
        let session_agent = self.handle.with(|s| s.agent.clone()).unwrap_or_else(|| self.agent.clone());
        let spinner = SPINNER[(self.frame / 2) % SPINNER.len()];
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            width,
            self.theme.name.as_str(),
            self.thinking_shown(),
            self.settings.grouping(),
            self.settings.tps(),
            &queued,
            perm_tool.as_deref(),
            self.permissions.len(),
            self.size.0,
            &self.shells,
            &base,
        )
            .hash(&mut h);
        let mut exp: Vec<&String> = self.expanded.iter().collect();
        exp.sort();
        exp.hash(&mut h);
        // Large conversations keep a static spinner: animating it would rebuild
        // every rendered line on each tick. Small sessions keep the animation.
        if message_count < 200 && (busy || !self.shells.is_empty()) {
            (self.frame / 2).hash(&mut h);
        }
        let key = h.finish();

        let app = self.app.clone();
        let app_models = app.clone();
        let model_cache = std::cell::RefCell::new(std::collections::HashMap::<String, Option<String>>::new());
        let model_name = |id: &str| {
            if let Some(name) = model_cache.borrow().get(id) {
                return name.clone();
            }
            let name = crate::models::resolve(&app_models.providers.read().unwrap(), id).ok().map(|r| r.model.name);
            model_cache.borrow_mut().insert(id.to_string(), name.clone());
            name
        };
        let child_cache = std::cell::RefCell::new(std::collections::HashMap::<String, bool>::new());
        let child_busy = |id: &str| {
            if let Some(busy) = child_cache.borrow().get(id) {
                return *busy;
            }
            let busy = app.session(id).map(|h| h.is_busy()).unwrap_or(false);
            child_cache.borrow_mut().insert(id.to_string(), busy);
            busy
        };
        let colors = self.theme.agent_colors();
        let order = self.agent_order();
        let fallback_color = colors.first().copied().unwrap_or(self.theme.base.text);
        let mut agent_map = std::collections::HashMap::new();
        for (i, id) in order.iter().enumerate() {
            let color = self
                .app
                .agent(id)
                .ok()
                .and_then(|a| a.color.as_deref().and_then(Rgba::from_hex))
                .unwrap_or_else(|| colors.get(i % colors.len().max(1)).copied().unwrap_or(fallback_color));
            agent_map.insert(id.clone(), color);
        }
        let agent_color = |id: &str| *agent_map.get(id).unwrap_or(&fallback_color);

        let session = self.handle.session.lock().unwrap();
        let len = session.messages.len();
        let default_start = len.saturating_sub(TRANSCRIPT_WINDOW);
        let prefix_len = len.saturating_sub(1);
        let start = self
            .cache
            .as_ref()
            .map(|cache| cache.start)
            .unwrap_or(default_start)
            .min(prefix_len);
        let prefix_key = transcript::hash_range(&session, start, prefix_len);
        let last_key = transcript::hash_range(&session, prefix_len, len);
        let ctx = transcript::Ctx {
            t: &self.theme,
            width,
            term_width: self.size.0 as usize,
            thinking: self.thinking_shown(),
            image_preview: self.settings.bool("session.image_preview").unwrap_or(false),
            grouping: self.settings.grouping(),
            tps: self.settings.tps(),
            spinner,
            expanded: &self.expanded,
            agent_color: &agent_color,
            model_name: &model_name,
            child_busy: &child_busy,
            base_dir: &base,
            home: &home,
            permission_tool: perm_tool.as_deref(),
            busy,
            session_agent: &session_agent,
        };

        if let Some(mut cache) = self.cache.take() {
            if cache.key == key {
                let total = cache.out.lines.len();
                if self.scroll + self.body_height + 2 < total {
                    cache.older_loaded = false;
                }
                if cache.prefix_len == prefix_len && cache.start > 0 && !cache.older_loaded && self.scroll + self.body_height >= total {
                    let new_start = cache.start.saturating_sub(TRANSCRIPT_WINDOW);
                    let prelude = transcript::render_range(&session, new_start, cache.start, &[], &ctx);
                    let added = prelude.lines.len();
                    let prelude_clicks = prelude.clicks.len();
                    let prelude_users = prelude.users.len();
                    if added > 0 {
                        for (a, b, _) in &mut cache.out.clicks {
                            *a += added;
                            *b += added;
                        }
                        for line in &mut cache.out.users {
                            *line += added;
                        }
                        let mut lines = prelude.lines;
                        lines.append(&mut cache.out.lines);
                        cache.out.lines = lines;
                        let mut clicks = prelude.clicks;
                        clicks.append(&mut cache.out.clicks);
                        cache.out.clicks = clicks;
                        let mut users = prelude.users;
                        users.append(&mut cache.out.users);
                        cache.out.users = users;
                        cache.base_lines += added;
                        cache.base_clicks += prelude_clicks;
                        cache.base_users += prelude_users;
                        cache.last_end_lines += added;
                        cache.last_end_clicks += prelude_clicks;
                        cache.last_end_users += prelude_users;
                        self.scroll += added;
                    }
                    cache.start = new_start;
                    cache.older_loaded = true;
                    cache.prefix_key = transcript::hash_range(&session, new_start, prefix_len);
                    self.cache = Some(cache);
                    return;
                }
                if cache.prefix_len == prefix_len && cache.prefix_key == prefix_key {
                    if cache.last_key == last_key && cache.busy == busy {
                        self.cache = Some(cache);
                        return;
                    }
                    cache.out.lines.truncate(cache.base_lines);
                    cache.out.clicks.truncate(cache.base_clicks);
                    cache.out.users.truncate(cache.base_users);
                    let suffix = transcript::render_range(&session, prefix_len, len, &queued, &ctx);
                    transcript::append(&mut cache.out, suffix);
                    cache.last_key = last_key;
                    cache.busy = busy;
                    cache.last_end_lines = cache.out.lines.len();
                    cache.last_end_clicks = cache.out.clicks.len();
                    cache.last_end_users = cache.out.users.len();
                    self.append_shells(&mut cache.out, spinner);
                    self.cache = Some(cache);
                    return;
                }
                // A new message arrived: the previous live suffix becomes part
                // of the cached prefix, so a long conversation is not rebuilt
                // on every prompt.
                if cache.prefix_len + 1 == prefix_len {
                    cache.out.lines.truncate(cache.last_end_lines);
                    cache.out.clicks.truncate(cache.last_end_clicks);
                    cache.out.users.truncate(cache.last_end_users);
                    cache.base_lines = cache.out.lines.len();
                    cache.base_clicks = cache.out.clicks.len();
                    cache.base_users = cache.out.users.len();
                    cache.prefix_len = prefix_len;
                    cache.prefix_key = prefix_key;
                    let suffix = transcript::render_range(&session, prefix_len, len, &queued, &ctx);
                    transcript::append(&mut cache.out, suffix);
                    cache.last_key = last_key;
                    cache.busy = busy;
                    cache.last_end_lines = cache.out.lines.len();
                    cache.last_end_clicks = cache.out.clicks.len();
                    cache.last_end_users = cache.out.users.len();
                    self.append_shells(&mut cache.out, spinner);
                    self.cache = Some(cache);
                    return;
                }
            }
        }

        let mut out = transcript::render_range(&session, start, prefix_len, &[], &ctx);
        let base_lines = out.lines.len();
        let base_clicks = out.clicks.len();
        let base_users = out.users.len();
        let suffix = transcript::render_range(&session, prefix_len, len, &queued, &ctx);
        transcript::append(&mut out, suffix);
        let last_end_lines = out.lines.len();
        let last_end_clicks = out.clicks.len();
        let last_end_users = out.users.len();
        drop(session);
        self.append_shells(&mut out, spinner);
        self.cache = Some(Cache {
            key,
            out,
            start,
            older_loaded: false,
            busy,
            prefix_len,
            prefix_key,
            base_lines,
            base_clicks,
            base_users,
            last_key,
            last_end_lines,
            last_end_clicks,
            last_end_users,
        });
    }

    fn append_shells(&self, out: &mut transcript::Out, spinner: &str) {
        let t = &self.theme;
        for c in &self.shells {
            let bg = t.raised;
            let stripe = span("┃", t.bg, bg);
            out.lines.push(draw::blank(t.bg));
            out.lines.push(line(vec![stripe.clone()], bg));
            out.lines.push(line(vec![stripe.clone(), span("  ", t.text, bg), span(format!("{spinner} {c}"), t.text, bg)], bg));
            out.lines.push(line(vec![stripe], bg));
        }
    }

    fn draw_sidebar(&self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let bg = t.raised;
        draw::fill(buf, area, bg);
        let inner_w = area.width.saturating_sub(4) as usize;
        let ts = draw::bold(draw::st(t.text, bg));
        let mut lines: Vec<Line<'static>> = Vec::new();
        let title = self.handle.with(|s| s.title.clone());
        for l in draw::wrap(&[Span::styled(title, ts)], inner_w.saturating_sub(2).max(1), draw::Wrap::Word) {
            lines.push(line(l, bg));
        }
        lines.push(line(vec![], bg));
        let (tokens, pct, cost) = self.handle.with(|s| {
            let last = s.messages.iter().rev().find(|m| m.role == Role::Assistant && (m.usage.input + m.usage.output) > 0);
            let tokens = last.map(|m| m.usage.input + m.usage.output + m.usage.reasoning + m.usage.cache_read + m.usage.cache_write).unwrap_or(0);
            let ctx = last.and_then(|m| m.model.as_deref()).and_then(|full| crate::models::resolve(&self.app.providers.read().unwrap(), full).ok()).map(|r| r.model.context).unwrap_or(0);
            let pct = (ctx > 0 && tokens > 0).then(|| ((tokens as f64 / ctx as f64) * 100.0).round() as u64);
            (tokens, pct, s.usage_total().1)
        });
        let mut sections: Vec<Vec<Line<'static>>> = Vec::new();
        if tokens > 0 || cost > 0.0 {
            let mut s = vec![line(vec![Span::styled("Context", ts)], bg)];
            if tokens > 0 {
                s.push(line(vec![span(format!("{} tokens", fmt_thousands(tokens)), t.muted, bg)], bg));
                if let Some(p) = pct {
                    s.push(line(vec![span(format!("{p}% used"), t.muted, bg)], bg));
                }
            }
            if cost > 0.0 {
                s.push(line(vec![span(format!("${cost:.2} spent"), t.muted, bg)], bg));
            }
            sections.push(s);
        }
        let servers = self.app.mcp.snapshot();
        if !servers.is_empty() {
            let mut s = vec![line(vec![Span::styled("MCP", ts)], bg)];
            for (n, st) in servers {
                let (dot, label, lc) = match st {
                    Some(crate::mcp::Status::Connected { .. }) => (t.success.base, "Connected", t.muted),
                    Some(crate::mcp::Status::Failed(_)) => (t.error.base, "Error", t.error.base),
                    Some(crate::mcp::Status::NeedsAuth(_)) => (t.warning.base, "Sign in", t.warning.base),
                    Some(crate::mcp::Status::Disabled) => (t.muted, "Disabled", t.muted),
                    None => (t.muted, "Connecting", t.muted),
                };
                let name_w = inner_w.saturating_sub(2 + 1 + label.len() + 1);
                let name = draw::truncate(&n, name_w);
                let gap = name_w.saturating_sub(draw::width(&name)) + 1;
                s.push(line(vec![span("• ", dot, bg), Span::styled(name, ts), span(" ".repeat(gap), t.text, bg), span(label, lc, bg)], bg));
            }
            sections.push(s);
        }
        let todos: Vec<(String, String)> = self.handle.with(|s| {
            s.messages
                .iter()
                .rev()
                .flat_map(|m| m.parts.iter().rev())
                .find_map(|p| match p {
                    Part::Tool { name, input, state: crate::session::ToolState::Completed { .. }, .. } if name == "todowrite" => Some(
                        input
                            .get("todos")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().map(|t| (t["content"].as_str().unwrap_or("").to_string(), t["status"].as_str().unwrap_or("pending").to_string())).collect())
                            .unwrap_or_default(),
                    ),
                    _ => None,
                })
                .unwrap_or_default()
        });
        if !todos.is_empty() {
            let mut s = vec![line(vec![Span::styled("Todo", ts)], bg)];
            for (c, st) in todos {
                let (mark, fg) = match st.as_str() {
                    "completed" => ("[✓]", t.muted),
                    "in_progress" => ("[•]", t.warning.base),
                    "cancelled" => ("[-]", t.faint),
                    _ => ("[ ]", t.muted),
                };
                for (i, l) in draw::wrap(&[span(c, fg, bg)], inner_w.saturating_sub(4).max(1), draw::Wrap::Word).into_iter().enumerate() {
                    let mut sp = vec![span(if i == 0 { format!("{mark} ") } else { "    ".into() }, fg, bg)];
                    sp.extend(l);
                    s.push(line(sp, bg));
                }
            }
            sections.push(s);
        }
        let n = sections.len();
        for (i, s) in sections.into_iter().enumerate() {
            lines.extend(s);
            if i + 1 < n {
                lines.push(line(vec![], bg));
            }
        }
        if !self.plugin_sidebar.is_empty() {
            if !lines.is_empty() {
                lines.push(line(vec![], bg));
            }
            for layout in &self.plugin_sidebar {
                lines.extend(plugin_ui::render(layout, inner_w.max(1), &t, bg));
            }
        }
        let body = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(4) };
        draw::draw_lines(buf, body, &lines, 0);
        let loc = draw::truncate_path(&self.location_label(), 38);
        draw::draw_line(buf, area.x + 2, area.y + area.height.saturating_sub(2), area.width.saturating_sub(4), &line(vec![span(loc, t.muted, bg)], bg));
    }

    /// `PermissionPrompt` / `SessionQuestion`: raised box with header, body and an options band.
    fn permission_lines(&self, req: &permission::Request, t: &Theme, width: usize) -> Vec<Line<'static>> {
        let bg = t.raised;
        let fbg = t.decrease(bg);
        let inner = width.saturating_sub(1 + 1 + 3 + 1).max(1);
        let input = self.handle.with(|s| {
            req.tool_call_id.as_ref().and_then(|id| {
                s.messages.iter().rev().flat_map(|m| m.parts.iter()).find_map(|p| match p {
                    Part::Tool { id: pid, input, .. } if pid == id => Some(input.clone()),
                    _ => None,
                })
            })
        });
        let input = input.unwrap_or(Value::Null);
        let home = crate::paths::get().home.to_string_lossy().to_string();
        let base = self.cwd().to_string_lossy().to_string();
        let fmt = |p: &str| draw::format_path(p, &base, &home);
        let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let action = match req.action.as_str() {
            "bash" => "shell",
            "task" => "subagent",
            "apply_patch" | "patch" | "write" => "edit",
            a => a,
        };
        let res0 = req.resources.first().cloned().unwrap_or_default();
        let pick = |k: &str| if s(k).is_empty() { res0.clone() } else { s(k) };
        let (icon, title, lines): (&str, String, Vec<String>) = match action {
            "edit" => ("→", format!("Edit {}", fmt(&pick("path"))), vec![]),
            "read" | "list" => ("→", format!("Read {}", fmt(&pick("path"))), vec![format!("Path: {}", fmt(&pick("path")))]),
            "glob" | "grep" => ("✱", format!("{} \"{}\"", titlecase(action), pick("pattern")), vec![format!("Pattern: {}", pick("pattern"))]),
            "shell" => {
                let c = s("command");
                ("#", "Shell command".into(), if c.is_empty() { req.resources.iter().map(|r| format!("- {r}")).collect() } else { vec![format!("$ {c}")] })
            }
            "subagent" => {
                let agent = if s("agent").is_empty() { "general".to_string() } else { s("agent") };
                ("#", format!("{} Subagent", titlecase(&agent)), if s("description").is_empty() { vec![] } else { vec![format!("◉ {}", s("description"))] })
            }
            "webfetch" => ("%", format!("WebFetch {}", s("url")), vec![format!("URL: {}", s("url"))]),
            "websearch" => ("◈", format!("Web Search \"{}\"", s("query")), vec![format!("Query: {}", s("query"))]),
            "external_directory" => ("←", format!("Access external directory {}", fmt(res0.trim_end_matches("/*").trim_end_matches("\\*"))), req.resources.iter().map(|r| format!("- {r}")).collect()),
            _ => ("⚙", format!("Call tool {}", req.action), vec![format!("Tool: {}", req.action)]),
        };
        let options = Self::permission_options(req);
        let selected = options.get(self.perm.selected).map(|o| o.1).unwrap_or(Reply::Once);
        let rejecting = self.perm.rejecting.as_ref();
        // the border sits on the panel, footer rows included
        let stripe = span("┃", if rejecting.is_some() { t.error.base } else { t.bg_primary.focused }, bg);
        let mut body: Vec<Line<'static>> = Vec::new();
        let indent = |l: Vec<Span<'static>>| {
            let mut sp = vec![span("  ", t.text, bg)];
            sp.extend(l);
            line(sp, bg)
        };
        if rejecting.is_some() {
            body.push(indent(vec![span("△ ", t.error.base, bg), span("Reject permission", t.text, bg)]));
            body.push(line(vec![], bg));
            body.push(indent(vec![span("Tell OpenCode what to do differently", t.muted, bg)]));
        } else {
            body.push(indent(vec![span("△ ", t.warning.base, bg), span("Permission required", t.text, bg)]));
            if action != "shell" && !title.is_empty() {
                body.push(indent(vec![span(format!("  {icon} "), t.muted, bg), span(draw::truncate(&title, inner.saturating_sub(6)), t.text, bg)]));
            }
            body.push(line(vec![], bg));
            if selected == Reply::Always {
                let save = &req.save;
                let ls: Vec<String> = if save.len() == 1 && save[0] == "*" {
                    vec![format!("This will always allow {} for this project.", req.action)]
                } else {
                    std::iter::once("This will always allow the following patterns for this project.".to_string()).chain(save.iter().map(|p| format!("- {p}"))).collect()
                };
                for (i, l) in ls.into_iter().enumerate() {
                    for w in draw::wrap(&[span(l, if i == 0 { t.muted } else { t.text }, bg)], inner, draw::Wrap::Word) {
                        body.push(indent(w));
                    }
                }
            } else if action == "edit" {
                let diff = req.metadata.get("diff").and_then(|d| d.as_str()).unwrap_or("");
                let ls: Vec<&str> = diff.lines().filter(|l| !l.starts_with("---") && !l.starts_with("+++")).collect();
                if ls.is_empty() {
                    body.push(indent(vec![span("No diff provided", t.muted, bg)]));
                }
                let max = if self.perm.expanded { 400 } else { 6 };
                for l in ls.iter().take(max) {
                    let (c, lbg) = if l.starts_with('+') {
                        (t.diff.highlight_added, t.diff.bg_added)
                    } else if l.starts_with('-') {
                        (t.diff.highlight_removed, t.diff.bg_removed)
                    } else if l.starts_with("@@") {
                        (t.diff.hunk_header, t.diff.bg_context)
                    } else {
                        (t.text, t.diff.bg_context)
                    };
                    let text = draw::take_width(&l.replace('\t', "    "), inner);
                    let pad = inner.saturating_sub(draw::width(&text));
                    body.push(indent(vec![span(text, c, lbg), span(" ".repeat(pad), t.text, lbg)]));
                }
            } else {
                let fg = if matches!(action, "shell" | "subagent") { t.text } else { t.muted };
                for l in lines {
                    for w in draw::wrap(&[span(l, fg, bg)], inner, draw::Wrap::Word) {
                        body.push(indent(w));
                    }
                }
            }
        }
        let max_body = if self.perm.expanded { (self.size.1 as usize).saturating_sub(10) } else { 9 };
        body.truncate(max_body.max(3));
        let mut out = vec![line(vec![stripe.clone()], bg)];
        for mut l in body {
            let mut sp = vec![stripe.clone()];
            sp.append(&mut l.spans);
            out.push(line(sp, bg));
        }
        out.push(line(vec![stripe.clone()], bg));
        let mut f: Vec<Span<'static>> = vec![stripe.clone(), span("  ", t.text, fbg)];
        if let Some(e) = rejecting {
            f.push(span(e.text.clone(), t.text, fbg));
            let hint = "enter confirm  esc cancel";
            let used: usize = f.iter().map(|s| draw::width(&s.content)).sum();
            f.push(span(" ".repeat(width.saturating_sub(used + hint.len() + 3)), t.text, fbg));
            f.extend([span("enter ", t.text, fbg), span("confirm", t.muted, fbg), span("  esc ", t.text, fbg), span("cancel", t.muted, fbg)]);
        } else {
            for (i, (label, r)) in options.iter().enumerate() {
                if i > 0 {
                    f.push(span(" ", t.text, fbg));
                }
                let sel = *r == selected;
                let obg = if sel { t.bg_primary.focused } else { t.bg_primary.base.over(fbg) };
                let ofg = if sel { t.text_primary.focused } else { t.text_primary.base };
                f.push(span(format!(" {label} "), ofg, obg));
            }
            let mut right = vec![span(format!("{} ", self.keys.label("permission.prompt.fullscreen").unwrap_or_default()), t.text, fbg), span(if self.perm.expanded { "minimize" } else { "fullscreen" }, t.muted, fbg)];
            right.extend([span("  ⇆ ", t.text, fbg), span("select", t.muted, fbg), span("  enter ", t.text, fbg), span("confirm", t.muted, fbg)]);
            let used: usize = f.iter().map(|s| draw::width(&s.content)).sum();
            let rw: usize = right.iter().map(|s| draw::width(&s.content)).sum();
            if used + rw + 3 <= width {
                f.push(span(" ".repeat(width - used - rw - 3), t.text, fbg));
                f.extend(right);
            }
        }
        out.push(line(vec![stripe.clone()], fbg));
        out.push(line(f, fbg));
        out.push(line(vec![stripe], fbg));
        out
    }

    fn draw_permission(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, req: &permission::Request) {
        let lines = self.permission_lines(req, t, area.width as usize);
        for (i, l) in lines.iter().enumerate() {
            draw::draw_line(buf, area.x, area.y + i as u16, area.width, l);
        }
        let y = area.y + lines.len() as u16 - 2;
        let mut x = area.x + 3;
        for (i, (o, _)) in Self::permission_options(req).iter().enumerate() {
            let w = o.len() as u16 + 2;
            self.hits.perm_options.push((Rect { x, y, width: w, height: 1 }, i));
            x += w + 1;
        }
    }

    fn draw_toast(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        self.toasts.retain(|x| x.until > Instant::now());
        let Some(toast) = self.toasts.front() else { return };
        let color = match toast.variant {
            Variant::Info => t.info.base,
            Variant::Success => t.success.base,
            Variant::Warning => t.warning.base,
            Variant::Error => t.error.base,
        };
        let bg = t.raised_high;
        let max_w = 60.min(area.width.saturating_sub(6)) as usize;
        let text_w = max_w.saturating_sub(2 + 4 + 3).max(1);
        let msg = draw::wrap(&[span(toast.message.clone(), t.text, bg)], text_w, draw::Wrap::Word);
        let mut body: Vec<Line<'static>> = Vec::new();
        if let Some(title) = &toast.title {
            body.push(line(vec![Span::styled(title.clone(), draw::bold(draw::st(t.text, bg)))], bg));
            body.push(line(vec![], bg));
        }
        body.extend(msg.into_iter().map(|l| line(l, bg)));
        let pending = self.toasts.len() - 1;
        if pending > 0 {
            body.push(line(vec![], bg));
            body.push(line(vec![span(format!("+{pending} more"), t.muted, bg)], bg));
        }
        let content_w = body.iter().map(draw::line_width).max().unwrap_or(0).min(text_w);
        let box_w = (content_w + 3 + 4 + 2) as u16;
        let x = area.x + area.width.saturating_sub(box_w + 2);
        let stripe = span("┃", color, t.bg);
        for i in 0..body.len() + 2 {
            let mut spans = vec![stripe.clone(), span("  ", t.text, bg)];
            if i > 0 && i <= body.len() {
                let mut l = body[i - 1].clone();
                let lw = draw::line_width(&l);
                spans.append(&mut l.spans);
                spans.push(span(" ".repeat(content_w.saturating_sub(lw)), t.text, bg));
                spans.push(span(if i == 1 { "  x" } else { "   " }, t.muted, bg));
            } else {
                spans.push(span(" ".repeat(content_w + 3), t.text, bg));
            }
            spans.push(span("  ", t.text, bg));
            spans.push(stripe.clone());
            draw::draw_line(buf, x, area.y + 1 + i as u16, box_w, &line(spans, bg));
        }
    }

    // ---------------------------------------------------------------- loop

    async fn event_loop(mut self, terminal: &mut DefaultTerminal, initial: Option<String>) -> Result<()> {
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_paused = paused.clone();
        std::thread::spawn(move || loop {
            if reader_paused.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            if let Ok(true) = event::poll(Duration::from_millis(50)) {
                match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        // connect MCP servers in the background so their status shows like upstream
        let app = self.app.clone();
        tokio::spawn(async move {
            app.mcp.tools().await;
            app.emit(AppEvent::Notice { message: String::new() });
        });
        self.refresh_plugin_slots().await;
        let mut events = self.app.subscribe();
        let mut tick = tokio::time::interval(TICK);
        if let Some(p) = initial {
            self.editor.set(&p);
            self.submit();
        }
        let mut dirty = true;
        loop {
            if self.pending_editor {
                self.pending_editor = false;
                paused.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(80));
                if let Some(t) = external_editor(terminal, &self.editor.text) {
                    self.editor.set(t.trim_end());
                }
                paused.store(false, std::sync::atomic::Ordering::SeqCst);
                terminal.clear()?;
                dirty = true;
            }
            self.persist_tabs();
            self.sync_chrome();
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }
            tokio::select! {
                Some(ev) = keys.recv() => {
                    let handle = |ui: &mut Ui, ev: Event| match ev {
                        Event::Key(k) if k.kind != KeyEventKind::Release => ui.on_key(k),
                        Event::Paste(s) => ui.on_paste(&s),
                        Event::Mouse(m) => ui.on_mouse(m),
                        _ => {}
                    };
                    handle(&mut self, ev);
                    while let Ok(ev) = keys.try_recv() {
                        handle(&mut self, ev);
                    }
                    dirty = true;
                }
                ev = events.recv() => {
                    if let Ok(ev) = ev {
                        self.on_app_event(ev);
                    }
                    dirty = true;
                }
                _ = tick.tick() => {
                    let animating = self.handle.is_busy() || !self.shells.is_empty() || self.tab_hover.is_some() || self.tabs.iter().any(|t| self.app.session(t).map(|h| h.is_busy()).unwrap_or(false));
                    let form = self.pending_form().map(|form| form.id).unwrap_or_default();
                    if form != self.form.id {
                        dirty = true;
                    }
                    if self.leader.is_some_and(|l| l.elapsed() >= LEADER_TIMEOUT) {
                        self.leader = None;
                        dirty = true;
                    }
                    if self.interrupt.is_some_and(|i| i.elapsed() >= Duration::from_secs(5)) {
                        self.interrupt = None;
                        dirty = true;
                    }
                    if animating || !self.toasts.is_empty() {
                        self.frame += 1;
                        dirty = true;
                    }
                    if self.refresh_plugin_slots().await {
                        dirty = true;
                    }
                }
            }
            if self.quit {
                break;
            }
        }
        Ok(())
    }
}

/// `prompt.editor`: edit the prompt in `$VISUAL`/`$EDITOR` (notepad on Windows).
fn external_editor(terminal: &mut DefaultTerminal, text: &str) -> Option<String> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "vi".into() });
    let path = std::env::temp_dir().join(format!("opencode-prompt-{}.md", std::process::id()));
    std::fs::write(&path, text).ok()?;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    let mut parts = editor.split_whitespace();
    let status = parts.next().map(|program| std::process::Command::new(program).args(parts).arg(&path).status());
    *terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let out = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    status.and_then(|s| s.ok()).filter(|s| s.success()).and(out)
}

/// Shared editing keys (`input.*` bindings).
fn edit_key(e: &mut Editor, k: &Key, ev: &KeyEvent, km: &Keymap, multiline: bool) {
    let m = |id: &str| km.matches(id, k, false);
    if multiline && m("input.newline") {
        e.insert("\n");
    } else if m("input.backspace") {
        e.backspace();
    } else if m("input.delete.word.backward") {
        e.delete_word_back();
    } else if m("input.delete.word.forward") {
        e.delete_word_forward();
    } else if m("input.delete.line") {
        e.delete_line();
    } else if m("input.delete.to.line.end") {
        e.delete_to_end();
    } else if m("input.delete.to.line.start") {
        e.delete_to_start();
    } else if m("input.delete") {
        e.delete();
    } else if m("input.word.backward") {
        e.word_left();
    } else if m("input.word.forward") {
        e.word_right();
    } else if m("input.move.left") {
        e.left();
    } else if m("input.move.right") {
        e.right();
    } else if m("input.line.home") || m("input.visual.line.home") {
        e.home();
    } else if m("input.line.end") || m("input.visual.line.end") {
        e.end();
    } else if m("input.undo") {
        e.undo();
    } else if m("input.redo") {
        e.redo();
    } else if let KeyCode::Char(c) = ev.code {
        if !ev.modifiers.contains(KeyModifiers::CONTROL) && !ev.modifiers.contains(KeyModifiers::ALT) {
            e.insert(c.encode_utf8(&mut [0; 4]));
        }
    }
}

pub async fn start(dir: std::path::PathBuf, opts: StartOptions) -> Result<()> {
    std::env::set_var("OPENCODE_TUI", "1");
    let app = App::init(dir).await?;
    let ui = Ui::new(app.clone(), &opts)?;
    let mouse = ui.settings.mouse();
    let mut terminal = ratatui::init();
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let res = ui.event_loop(&mut terminal, opts.prompt.clone()).await;
    app.plugins.flush().await;
    app.plugins.shutdown().await;
    // clear the title and give the terminal its own background back
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste, ratatui::crossterm::style::Print("\x1b]0;\x07\x1b]111\x1b\\"));
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests;
