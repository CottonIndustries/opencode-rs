//! Session transcript, laid out like upstream's `routes/session`: one row per message entry
//! (separated by a blank line), inline tool rows, raised block tools, reasoning blocks,
//! exploration/thought groups and per-turn assistant footers.

use super::draw::{self, line, span, Wrap};
use super::highlight;
use super::markdown;
use super::theme::{Rgba, Theme};
use crate::session::{Message, Part, Role, Session, ToolState};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::HashSet;

pub const ICON_WIDTH: usize = 2;

pub struct Ctx<'a> {
    pub t: &'a Theme,
    /// Content width (session pane minus its side padding).
    pub width: usize,
    pub term_width: usize,
    pub thinking: bool,
    pub grouping: bool,
    pub tps: bool,
    pub spinner: &'a str,
    pub expanded: &'a HashSet<String>,
    pub agent_color: &'a dyn Fn(&str) -> Rgba,
    pub model_name: &'a dyn Fn(&str) -> Option<String>,
    pub child_busy: &'a dyn Fn(&str) -> bool,
    pub base_dir: &'a str,
    pub home: &'a str,
    pub permission_tool: Option<&'a str>,
    pub busy: bool,
    pub session_agent: &'a str,
}

/// Rendered transcript plus clickable line ranges (toggle ids).
#[derive(Default)]
pub struct Out {
    pub lines: Vec<Line<'static>>,
    pub clicks: Vec<(usize, usize, String)>,
    /// First line of each user message, for message navigation.
    pub users: Vec<usize>,
}

struct Row {
    lines: Vec<Line<'static>>,
    click: Option<String>,
    user: bool,
    /// Rows rendered inside a group follow their header without a blank line.
    tight: bool,
}

impl Row {
    fn new(lines: Vec<Line<'static>>) -> Row {
        Row { lines, click: None, user: false, tight: false }
    }
    fn click(mut self, id: impl Into<String>) -> Row {
        self.click = Some(id.into());
        self
    }
}

enum Entry<'a> {
    Note(String),
    User(&'a Message),
    Notice(String, String, Rgba),
    Compaction(&'a Message),
    Text(&'a Message, &'a str),
    Reasoning(&'a Message, usize, &'a str),
    Tool(&'a Message, &'a Part),
    Footer(&'a Message, usize),
    /// A command run from the prompt's shell mode: (command, exit code, output).
    Shell(String, i64, String),
}

fn s<'v>(v: &'v Value, k: &str) -> Option<&'v str> {
    v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty())
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

/// `Locale.duration`.
pub fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    if ms < 3_600_000 {
        return format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000);
    }
    if ms < 86_400_000 {
        return format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000);
    }
    format!("{}d {}h", ms / 86_400_000, (ms % 86_400_000) / 3_600_000)
}

/// `Locale.number`.
pub fn number(n: u64) -> String {
    if n >= 999_950 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn exploration(name: &str) -> bool {
    matches!(name, "read" | "glob" | "grep" | "webfetch" | "websearch" | "list")
}

fn reasoning_content(text: &str) -> &str {
    text.trim()
}

fn reasoning_title(text: &str) -> Option<String> {
    let t = text.trim();
    let rest = t.strip_prefix("**")?;
    let end = rest.find("**")?;
    let title = &rest[..end];
    if title.contains('\n') || title.contains('*') {
        return None;
    }
    let after = &rest[end + 2..];
    if after.is_empty() || after.starts_with("\n\n") || after.starts_with("\r\n\r\n") {
        Some(title.trim().to_string())
    } else {
        None
    }
}

fn terminal_finish(m: &Message) -> bool {
    m.error.is_some() || m.finish.as_deref().is_some_and(|f| !matches!(f, "tool-calls" | "tool_calls" | "tool_use" | "unknown" | ""))
}

fn interrupted(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("interrupt") || e.contains("aborted") || e.contains("cancelled") || e == "step interrupted"
}

impl Ctx<'_> {
    fn bg(&self) -> Rgba {
        self.t.bg
    }

    fn path(&self, p: &str) -> String {
        draw::format_path(p, self.base_dir, self.home)
    }

    fn model_label(&self, full: &str) -> String {
        let base = full.split('#').next().unwrap_or(full);
        (self.model_name)(base).unwrap_or_else(|| base.to_string())
    }

    /// `InlineToolRow`: paddingLeft 3, a 2-column icon (or spinner), wrapped label.
    fn inline(&self, icon: &str, text: &str, color: Rgba, spinner: bool, strike: bool) -> Vec<Line<'static>> {
        let bg = self.bg();
        let mut st = draw::st(color, bg);
        if strike {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        let avail = self.width.saturating_sub(3 + ICON_WIDTH).max(1);
        let wrapped = draw::wrap(&[Span::styled(text.to_string(), st)], avail, Wrap::Word);
        let mut out = Vec::new();
        for (i, l) in wrapped.into_iter().enumerate() {
            let mut spans = vec![span("   ", color, bg)];
            if i == 0 {
                let glyph = if spinner { self.spinner } else { icon };
                let pad = ICON_WIDTH.saturating_sub(draw::width(glyph));
                spans.push(Span::styled(format!("{glyph}{}", " ".repeat(pad)), st));
            } else {
                spans.push(span("  ", color, bg));
            }
            spans.extend(l);
            out.push(line(spans, bg));
        }
        out
    }

    /// Raised block with a left stripe in the page background (`BlockTool`).
    fn block(&self, children: Vec<Vec<Line<'static>>>, hover: bool) -> Vec<Line<'static>> {
        let t = self.t;
        let bg = if hover { t.decrease(t.raised) } else { t.raised };
        let stripe = span("┃", t.bg, bg);
        let wrap_line = |mut l: Line<'static>| {
            let mut spans = vec![stripe.clone(), span("  ", t.text, bg)];
            spans.append(&mut l.spans);
            line(spans, bg)
        };
        let mut out = vec![line(vec![stripe.clone()], bg)];
        let children: Vec<_> = children.into_iter().filter(|c| !c.is_empty()).collect();
        let n = children.len();
        for (i, c) in children.into_iter().enumerate() {
            out.extend(c.into_iter().map(wrap_line));
            if i + 1 < n {
                out.push(line(vec![stripe.clone()], bg));
            }
        }
        out.push(line(vec![stripe], bg));
        out
    }

    fn block_width(&self) -> usize {
        self.width.saturating_sub(5).max(1)
    }

    fn header(&self, label: &str, value: Option<&str>, color: Rgba, spinner: bool) -> Vec<Line<'static>> {
        let bg = self.t.raised;
        let label = if spinner { format!("{} {}", self.spinner, label.trim_start_matches("# ")) } else { label.to_string() };
        let mut spans = vec![span(label.clone(), color, bg)];
        if let Some(v) = value {
            let max = self.width.saturating_sub(6 + draw::width(&label)).max(2);
            spans.push(span(format!(" {}", draw::truncate_path(v, max)), color, bg));
        }
        vec![line(spans, bg)]
    }

    fn text_lines(&self, text: &str, color: Rgba, width: usize, mode: Wrap, bg: Rgba) -> Vec<Line<'static>> {
        draw::paragraph(&[span(text.to_string(), color, bg)], width, mode, bg)
    }
}

pub fn render(session: &Session, queued: &[String], ctx: &Ctx) -> Out {
    render_range(session, 0, session.messages.len(), queued, ctx)
}

/// Render a contiguous message range. The TUI keeps the prefix of a large
/// conversation cached and only re-renders the last message while it streams.
pub fn render_range(session: &Session, start: usize, end: usize, queued: &[String], ctx: &Ctx) -> Out {
    let entries = entries_range(session, start, end, ctx);
    let mut rows: Vec<Row> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        // groups: consecutive reasoning (when hidden) or exploration tools
        let kind = group_kind(&entries[i], ctx);
        if let Some(k) = kind {
            let mut j = i;
            while j < entries.len() && group_kind(&entries[j], ctx) == Some(k) {
                j += 1;
            }
            let completed = j < entries.len();
            rows.extend(group(&entries[i..j], k, completed, ctx));
            i = j;
            continue;
        }
        rows.extend(entry(&entries[i], session, ctx));
        i += 1;
    }
    for q in queued {
        rows.push(user_box(q, &[], ctx.t.border, ctx));
    }
    let mut out = Out::default();
    for r in rows {
        if !r.tight {
            out.lines.push(draw::blank(ctx.bg()));
        }
        let start = out.lines.len();
        if r.user {
            out.users.push(start);
        }
        out.lines.extend(r.lines);
        if let Some(id) = r.click {
            out.clicks.push((start, out.lines.len(), id));
        }
    }
    out
}

/// Append a rendered suffix, shifting its clickable and user-message offsets.
pub fn append(base: &mut Out, part: Out) {
    let offset = base.lines.len();
    base.lines.extend(part.lines);
    base.clicks.extend(part.clicks.into_iter().map(|(a, b, id)| (a + offset, b + offset, id)));
    base.users.extend(part.users.into_iter().map(|line| line + offset));
}

/// Cheap content fingerprint for a message range. Deep ranges (the live last
/// message) include text and tool output; long prefixes only need stable
/// identity and lengths because completed history does not change.
pub fn hash_range(session: &Session, start: usize, end: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let deep = end.saturating_sub(start) <= 8;
    for m in session.messages.iter().take(end).skip(start) {
        m.id.hash(&mut h);
        matches!(m.role, Role::User).hash(&mut h);
        m.parts.len().hash(&mut h);
        m.completed.hash(&mut h);
        m.model.hash(&mut h);
        m.agent.hash(&mut h);
        m.finish.hash(&mut h);
        m.error.hash(&mut h);
        m.usage.input.hash(&mut h);
        m.usage.output.hash(&mut h);
        m.usage.reasoning.hash(&mut h);
        m.usage.cache_read.hash(&mut h);
        m.usage.cache_write.hash(&mut h);
        m.cost.to_bits().hash(&mut h);
        for p in &m.parts {
            match p {
                Part::Text { text, synthetic } => {
                    synthetic.hash(&mut h);
                    if deep { text.hash(&mut h) } else { text.len().hash(&mut h) }
                }
                Part::Reasoning { text, .. } => {
                    if deep { text.hash(&mut h) } else { text.len().hash(&mut h) }
                }
                Part::Tool { id, name, input, state, .. } => {
                    id.hash(&mut h);
                    name.hash(&mut h);
                    if deep {
                        input.to_string().hash(&mut h);
                    } else {
                        input.as_object().map_or(0, |object| object.len()).hash(&mut h);
                    }
                    match state {
                        ToolState::Pending => 0u8.hash(&mut h),
                        ToolState::Running => 1u8.hash(&mut h),
                        ToolState::Completed { output, metadata, .. } => {
                            2u8.hash(&mut h);
                            if deep { output.hash(&mut h) } else { output.len().hash(&mut h) }
                            if deep {
                                metadata.to_string().hash(&mut h);
                            } else {
                                metadata.as_object().map_or(0, |object| object.len()).hash(&mut h);
                            }
                        }
                        ToolState::Error { error } => {
                            3u8.hash(&mut h);
                            error.hash(&mut h);
                        }
                    }
                }
                Part::File(file) => {
                    file.mime.hash(&mut h);
                    file.name.hash(&mut h);
                    file.data.len().hash(&mut h);
                }
                Part::Compaction { summary, .. } => {
                    if deep { summary.hash(&mut h) } else { summary.len().hash(&mut h) }
                }
            }
        }
    }
    h.finish()
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    Reasoning,
    Exploration,
}

fn group_kind(e: &Entry, ctx: &Ctx) -> Option<Group> {
    match e {
        Entry::Reasoning(..) if !ctx.thinking => Some(Group::Reasoning),
        Entry::Tool(_, Part::Tool { name, .. }) if ctx.grouping && exploration(name) => Some(Group::Exploration),
        _ => None,
    }
}

fn entries_range<'a>(session: &'a Session, start: usize, end: usize, ctx: &Ctx) -> Vec<Entry<'a>> {
    let msgs = &session.messages;
    let mut out = Vec::new();
    let mut prev_model: Option<&str> = None;
    let mut prev_agent: Option<&str> = None;
    for m in msgs.iter().take(start) {
        if let Some(model) = m.model.as_deref() {
            prev_model = Some(model);
        }
        if let Some(agent) = m.agent.as_deref() {
            prev_agent = Some(agent);
        }
    }
    for (idx, m) in msgs.iter().enumerate().take(end).skip(start) {
        match m.role {
            Role::User => {
                if m.parts.iter().any(|p| matches!(p, Part::Compaction { .. })) {
                    out.push(Entry::Compaction(m));
                    continue;
                }
                let real = m.parts.iter().any(|p| matches!(p, Part::Text { synthetic: false, text } if !text.trim().is_empty()) || matches!(p, Part::File(_)));
                if !real {
                    if let Some(n) = notice(m, ctx) {
                        out.push(n);
                    }
                    continue;
                }
                if let Some(model) = m.model.as_deref() {
                    let (base, variant) = model.split_once('#').map(|(a, b)| (a, Some(b))).unwrap_or((model, None));
                    match prev_model {
                        Some(p) if p == model => {}
                        Some(p) if p.split('#').next() == Some(base) => out.push(Entry::Note(format!("Switched variant to {}", variant.unwrap_or("default")))),
                        // the session's initial model is not a switch
                        None => {}
                        _ => {
                            out.push(Entry::Note(switch_label(base, ctx)));
                            if let Some(v) = variant {
                                if let Some(Entry::Note(n)) = out.last_mut() {
                                    if !n.contains('/') {
                                        n.push_str(&format!(" ({v})"));
                                    }
                                }
                            }
                        }
                    }
                    prev_model = Some(model);
                }
                if let (Some(a), Some(p)) = (m.agent.as_deref(), prev_agent) {
                    if a != p {
                        out.push(Entry::Note(format!("Switched agent from {} to {}", titlecase(p), titlecase(a))));
                    }
                }
                if m.agent.is_some() {
                    prev_agent = m.agent.as_deref();
                }
                out.push(Entry::User(m));
            }
            Role::Assistant => {
                let mut reasoning = 0;
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.trim().is_empty() => out.push(Entry::Text(m, text)),
                        Part::Reasoning { text, .. } if !reasoning_content(text).is_empty() => {
                            out.push(Entry::Reasoning(m, reasoning, text));
                            reasoning += 1;
                        }
                        Part::Tool { .. } => out.push(Entry::Tool(m, p)),
                        _ => {}
                    }
                }
                if terminal_finish(m) || (m.completed.is_none() && m.error.is_some()) {
                    out.push(Entry::Footer(m, idx));
                }
            }
        }
    }
    out
}

fn switch_label(model: &str, ctx: &Ctx) -> String {
    match (ctx.model_name)(model) {
        Some(name) => format!("Switched model to {name}"),
        None => format!("Switched model to {model}"),
    }
}

fn attr<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let start = text.find(&pat)? + pat.len();
    let end = text[start..].find('"')? + start;
    Some(&text[start..end])
}

/// Background shell/subagent completions arrive as synthetic user messages.
fn notice<'a>(m: &'a Message, ctx: &Ctx) -> Option<Entry<'a>> {
    let text = m.text();
    let first = text.trim_start();
    let t = ctx.t;
    if first.starts_with("<shell-run") {
        let command = attr(first, "command").unwrap_or("").replace("&quot;", "\"");
        let exit = attr(first, "exit").and_then(|e| e.parse().ok()).unwrap_or(0);
        let body = first.split_once('\n').map(|(_, b)| b).unwrap_or("");
        let output = body.trim_end().trim_end_matches("</shell-run>").trim_end().to_string();
        return Some(Entry::Shell(command, exit, output));
    }
    let (actor, rest) = if first.starts_with("<shell") {
        ("Shell".to_string(), attr(first, "command").unwrap_or("").replace("&quot;", "\"").split_whitespace().collect::<Vec<_>>().join(" "))
    } else if first.starts_with("<subagent") {
        let body = first.split_once('\n').map(|(_, b)| b).unwrap_or("");
        let body = body.trim_end().trim_end_matches("</subagent>").trim();
        ("Subagent".to_string(), body.lines().next().unwrap_or("").to_string())
    } else {
        return None;
    };
    let state = attr(first, "state").unwrap_or("completed");
    let (icon, status, color) = match state {
        "completed" => ("↳", "finished", t.info.base),
        "error" => ("!", "failed", t.error.base),
        "cancelled" => ("!", "cancelled", t.warning.base),
        other => ("!", other, t.info.base),
    };
    Some(Entry::Notice(format!("{icon} {actor} {status}"), rest, color))
}

fn entry(e: &Entry, session: &Session, ctx: &Ctx) -> Vec<Row> {
    let t = ctx.t;
    let bg = t.bg;
    match e {
        Entry::Note(text) => vec![Row::new(indent3(ctx.text_lines(text, t.muted, ctx.width.saturating_sub(3), Wrap::Word, bg), bg))],
        Entry::Notice(heading, rest, color) => {
            let suffix = draw::truncate(&format!(" · {rest}"), ctx.width.saturating_sub(3 + draw::width(heading)));
            let suffix = if rest.is_empty() { String::new() } else { suffix };
            vec![Row::new(vec![line(vec![span("   ", t.text, bg), span(heading.clone(), *color, bg), span(suffix, t.muted, bg)], bg)])]
        }
        Entry::User(m) => {
            let text = m.parts.iter().filter_map(|p| if let Part::Text { text, synthetic: false } = p { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
            let files = attachments(m, ctx);
            let color = (ctx.agent_color)(m.agent.as_deref().unwrap_or(ctx.session_agent));
            let mut r = user_box(&text, &files, color, ctx);
            r.user = true;
            vec![r]
        }
        Entry::Compaction(m) => vec![Row::new(compaction(m, ctx))],
        Entry::Text(m, text) => {
            let _ = m;
            vec![Row::new(indent3(markdown::render(text.trim(), ctx.width.saturating_sub(3), t, bg), bg))]
        }
        Entry::Reasoning(m, i, text) => vec![Row::new(reasoning(m, *i, text, ctx))],
        Entry::Tool(m, p) => tool(m, p, ctx),
        Entry::Footer(m, idx) => vec![Row::new(footer(session, m, *idx, ctx))],
        Entry::Shell(command, exit, output) => {
            let id = format!("shell-run:{}:{}", command.len(), output.len());
            let input = serde_json::json!({ "command": command });
            let state = if *exit == 0 {
                ToolState::Completed { output: output.clone(), title: String::new(), metadata: Value::Null, attachments: vec![] }
            } else {
                ToolState::Error { error: format!("{output}\n\nCommand exited with code {exit}").trim().to_string() }
            };
            vec![shell(&id, &input, &state, &Value::Null, false, ctx.expanded.contains(&id), ctx)]
        }
    }
}

fn indent3(lines: Vec<Line<'static>>, bg: Rgba) -> Vec<Line<'static>> {
    draw::indent(lines, &[span("   ", Rgba::BLACK, bg)])
}

fn attachments(m: &Message, ctx: &Ctx) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text { text, synthetic: true } => {
                let t = text.trim_start();
                if let Some(rest) = t.strip_prefix("Attached file: ") {
                    out.push(("file".into(), ctx.path(rest.lines().next().unwrap_or(""))));
                } else if let Some(rest) = t.strip_prefix("Attached directory: ") {
                    out.push(("dir".into(), ctx.path(rest.lines().next().unwrap_or(""))));
                }
            }
            Part::File(f) => {
                let label = if f.mime == "application/x-directory" { "dir" } else { "file" };
                if !out.iter().any(|(_, n)| *n == f.name) {
                    out.push((label.into(), if f.name.is_empty() { "attachment".into() } else { f.name.clone() }));
                }
            }
            _ => {}
        }
    }
    // an image carries both a text note and the file part
    out.dedup_by(|a, b| a.1 == b.1 || std::path::Path::new(&a.1).file_name() == std::path::Path::new(&b.1).file_name());
    out
}

/// `UserMessage`: raised box with an agent-colored stripe.
fn user_box(text: &str, files: &[(String, String)], color: Rgba, ctx: &Ctx) -> Row {
    let t = ctx.t;
    let bg = t.raised;
    let stripe = span("┃", color, bg);
    let inner = ctx.width.saturating_sub(5).max(1);
    let mut body = ctx.text_lines(text.trim_end(), t.text, inner, Wrap::Word, bg);
    if text.trim().is_empty() {
        body.clear();
    }
    if !files.is_empty() {
        if !body.is_empty() {
            body.push(line(vec![], bg));
        }
        let chip_bg = t.hue("accent", if t.mode == super::theme::Mode::Light { 300 } else { 200 });
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (label, name) in files {
            let chip = vec![Span::styled(format!(" {label} "), draw::bold(draw::st(t.raised, chip_bg))), span(format!(" {name} "), t.muted, t.decrease(bg))];
            let w: usize = chip.iter().map(|s| draw::width(&s.content)).sum();
            if used > 0 && used + 1 + w > inner {
                body.push(line(std::mem::take(&mut row), bg));
                used = 0;
            }
            if used > 0 {
                row.push(span(" ", t.text, bg));
                used += 1;
            }
            row.extend(chip);
            used += w;
        }
        if !row.is_empty() {
            body.push(line(row, bg));
        }
    }
    let mut lines = vec![line(vec![stripe.clone()], bg)];
    for mut l in body {
        let mut spans = vec![stripe.clone(), span("  ", t.text, bg)];
        spans.append(&mut l.spans);
        lines.push(line(spans, bg));
    }
    lines.push(line(vec![stripe], bg));
    Row::new(lines)
}

fn compaction(m: &Message, ctx: &Ctx) -> Vec<Line<'static>> {
    let t = ctx.t;
    let bg = t.bg;
    let summary = m.parts.iter().find_map(|p| if let Part::Compaction { summary, .. } = p { Some(summary.as_str()) } else { None }).unwrap_or("");
    let u = &m.usage;
    let input = u.input + u.cache_read + u.cache_write;
    let output = u.output + u.reasoning;
    let mut label = "Compaction".to_string();
    if input + output > 0 {
        label.push_str(&format!(" · {} in · {} out", number(input), number(output)));
    }
    let lw = draw::width(&label) + 2;
    let side = ctx.width.saturating_sub(lw);
    let left = side / 2;
    let right = side - left;
    let mut out = vec![line(vec![span("─".repeat(left), t.muted, bg), span(format!(" {label} "), t.muted, bg), span("─".repeat(right), t.muted, bg)], bg)];
    if !summary.trim().is_empty() {
        out.push(draw::blank(bg));
        out.extend(indent3(markdown::render(summary.trim(), ctx.width.saturating_sub(3), t, bg), bg));
    }
    out
}

fn reasoning_done(m: &Message, ctx: &Ctx) -> bool {
    m.completed.is_some() || !ctx.busy || m.parts.iter().any(|p| matches!(p, Part::Text { .. } | Part::Tool { .. }))
}

fn reasoning_duration(m: &Message, i: usize) -> Option<i64> {
    let mut n = 0;
    for p in &m.parts {
        if let Part::Reasoning { text, started, ended, .. } = p {
            if reasoning_content(text).is_empty() {
                continue;
            }
            if n == i {
                let start = started.unwrap_or(m.time);
                let end = ended.or(m.completed)?;
                return Some((end - start).max(0));
            }
            n += 1;
        }
    }
    None
}

/// `ReasoningPart` with thinking shown: header, then the thought behind a stripe.
fn reasoning(m: &Message, i: usize, text: &str, ctx: &Ctx) -> Vec<Line<'static>> {
    let t = ctx.t;
    let bg = t.bg;
    let bar = span("┃", t.decrease(t.bg), bg);
    let warn = t.warning.base.with_alpha(0.6);
    let done = reasoning_done(m, ctx);
    let header = if !done {
        format!("{} Thinking", ctx.spinner)
    } else {
        match reasoning_duration(m, i) {
            Some(d) => format!("Thought: {}", duration(d)),
            None => "Thought".into(),
        }
    };
    let mut out = vec![line(vec![span("   ", t.text, bg), bar.clone(), span(" ", t.text, bg), span(header, warn, bg)], bg)];
    out.push(draw::blank(bg));
    let body = markdown::render_with(reasoning_content(text), ctx.width.saturating_sub(5).max(1), t, bg, t.muted);
    for mut l in body {
        let mut spans = vec![span("   ", t.text, bg), bar.clone(), span(" ", t.text, bg)];
        spans.append(&mut l.spans);
        out.push(line(spans, bg));
    }
    out
}

fn group(es: &[Entry], kind: Group, completed: bool, ctx: &Ctx) -> Vec<Row> {
    let t = ctx.t;
    let bg = t.bg;
    let id = match &es[0] {
        Entry::Tool(_, Part::Tool { id, .. }) => format!("group:{id}"),
        Entry::Reasoning(m, i, _) => format!("group:{}:{i}", m.id),
        _ => "group".into(),
    };
    let expanded = ctx.expanded.contains(&id);
    match kind {
        Group::Exploration => {
            // a tool awaiting permission leaves the summary and renders as its own row after it
            let (pending, tools): (Vec<(&Message, &Part)>, Vec<(&Message, &Part)>) = es
                .iter()
                .filter_map(|e| if let Entry::Tool(m, p) = e { Some((*m, *p)) } else { None })
                .partition(|(_, p)| matches!(p, Part::Tool { id, .. } if ctx.permission_tool == Some(id.as_str())));
            let done = completed || tools.iter().all(|(_, p)| matches!(p, Part::Tool { state: ToolState::Completed { .. } | ToolState::Error { .. }, .. }));
            let mut counts: Vec<(String, usize)> = Vec::new();
            for (_, p) in &tools {
                if let Part::Tool { name, .. } = p {
                    let n = match name.as_str() {
                        "grep" | "glob" | "websearch" => "search",
                        "webfetch" => "fetch",
                        other => other,
                    };
                    match counts.iter_mut().find(|(k, _)| k == n) {
                        Some((_, c)) => *c += 1,
                        None => counts.push((n.to_string(), 1)),
                    }
                }
            }
            let names: Vec<String> = counts
                .iter()
                .map(|(n, c)| {
                    let plural = if *c == 1 { n.clone() } else if n == "search" || n == "fetch" { format!("{n}es") } else { format!("{n}s") };
                    format!("{c} {plural}")
                })
                .collect();
            let label = format!("{}: {}", if done { "Explored" } else { "Exploring" }, names.join(", "));
            let mut rows = Vec::new();
            if !tools.is_empty() {
                rows.push(Row::new(ctx.inline(if done { "→" } else { "✱" }, &label, t.muted, !done, false)).click(id));
            }
            if expanded {
                for (m, p) in tools {
                    for mut r in tool(m, p, ctx) {
                        r.tight = true;
                        rows.push(r);
                    }
                }
            }
            for (m, p) in pending {
                rows.extend(tool(m, p, ctx));
            }
            rows
        }
        Group::Reasoning => {
            let thoughts: Vec<(&Message, usize, &str)> = es.iter().filter_map(|e| if let Entry::Reasoning(m, i, s) = e { Some((*m, *i, *s)) } else { None }).collect();
            if let (1, false) = (thoughts.len(), expanded) {
                let (m, i, s) = thoughts[0];
                return vec![Row::new(reasoning(m, i, s, ctx))];
            }
            let done = completed || thoughts.iter().all(|(m, _, _)| reasoning_done(m, ctx));
            let latest = thoughts.iter().rev().find_map(|(_, _, s)| reasoning_title(s));
            let total: i64 = thoughts.iter().filter_map(|(m, i, _)| reasoning_duration(m, *i)).sum();
            let color = if !done {
                t.text
            } else if expanded {
                t.warning.base
            } else {
                t.warning.base.with_alpha(0.6)
            };
            let text = if done {
                let mut s = "Thought".to_string();
                if let (false, Some(l)) = (expanded, &latest) {
                    s.push_str(&format!(": {l}"));
                }
                if thoughts.len() > 1 {
                    s.push_str(&format!(" · {} steps", thoughts.len()));
                }
                if total > 0 {
                    s.push_str(&format!(" · {}", duration(total)));
                }
                s
            } else {
                match &latest {
                    Some(l) => format!("Thinking: {l}"),
                    None => "Thinking".into(),
                }
            };
            let mut rows = vec![Row::new(ctx.inline(if expanded { "-" } else { "+" }, &text, color, !done, false)).click(id)];
            if expanded {
                let bar = span("┃", t.decrease(t.raised), bg);
                let mut lines = Vec::new();
                for (_, _, s) in thoughts {
                    lines.push(draw::blank(bg));
                    for mut l in markdown::render_with(reasoning_content(s), ctx.width.saturating_sub(8).max(1), t, bg, t.muted) {
                        let mut spans = vec![span("      ", t.text, bg), bar.clone(), span(" ", t.text, bg)];
                        spans.append(&mut l.spans);
                        lines.push(line(spans, bg));
                    }
                }
                let mut r = Row::new(lines);
                r.tight = true;
                rows.push(r);
            }
            rows
        }
    }
}

fn footer(session: &Session, m: &Message, idx: usize, ctx: &Ctx) -> Vec<Line<'static>> {
    let t = ctx.t;
    let bg = t.bg;
    let mut out = Vec::new();
    let err = m.error.as_deref().filter(|e| !interrupted(e));
    if let Some(e) = err {
        out.extend(indent3(ctx.text_lines(&format!("Error: {e}"), t.error.base, ctx.width.saturating_sub(3), Wrap::Word, bg), bg));
        out.push(draw::blank(bg));
    }
    let agent = m.agent.as_deref().unwrap_or(ctx.session_agent);
    let mut spans = vec![span("   ", t.text, bg), span(titlecase(agent), if m.error.is_some() { t.muted } else { (ctx.agent_color)(agent) }, bg)];
    if ctx.term_width >= 28 {
        spans.push(span(format!(" · {}", ctx.model_label(m.model.as_deref().unwrap_or("unknown"))), t.muted, bg));
    }
    let (dur, tps) = turn_stats(session, idx);
    if dur > 0 && (ctx.term_width < 28 || ctx.term_width >= 36) {
        spans.push(span(format!(" · {}", duration(dur)), t.muted, bg));
    }
    if ctx.tps {
        if let Some(v) = tps {
            spans.push(span(format!(" · {v:.1} tok/s"), t.muted, bg));
        }
    }
    if m.error.as_deref().is_some_and(interrupted) {
        spans.push(span(" · interrupted", t.muted, bg));
    }
    out.push(line(spans, bg));
    out
}

/// `SessionTurn.duration` / `tokensPerSecond`: from the input that started the turn to this step.
fn turn_stats(session: &Session, idx: usize) -> (i64, Option<f64>) {
    let msgs = &session.messages;
    let m = &msgs[idx];
    let start = (0..idx).rev().find(|i| msgs[*i].role == Role::User).unwrap_or(0);
    let input_time = msgs.get(start).filter(|x| x.role == Role::User).map(|x| x.time).unwrap_or(m.time);
    let dur = m.completed.map(|c| (c - input_time).max(0)).unwrap_or(0);
    let mut output = 0u64;
    let mut total = 0i64;
    for s in msgs[start..=idx].iter().filter(|x| x.role == Role::Assistant) {
        let Some(done) = s.completed else { return (dur, None) };
        total += (done - s.time).max(0);
        output += s.usage.output + s.usage.reasoning;
    }
    let tps = if output > 0 && total > 0 { Some(output as f64 / (total as f64 / 1000.0)) } else { None };
    (dur, tps)
}

// ---------------------------------------------------------------- tools

fn state_err(state: &ToolState) -> Option<&str> {
    match state {
        ToolState::Error { error } => Some(error),
        _ => None,
    }
}

fn denied(err: &str) -> bool {
    err.contains("rejected permission") || err.contains("specified a rule") || err.contains("user dismissed") || err.contains("Permission rejected") || err.contains(crate::engine::REJECTED)
}

fn tool(m: &Message, p: &Part, ctx: &Ctx) -> Vec<Row> {
    let Part::Tool { id, name, input, state, .. } = p else { return vec![] };
    let _ = m;
    let t = ctx.t;
    let pending_perm = ctx.permission_tool == Some(id.as_str());
    let running = matches!(state, ToolState::Running);
    let streaming = matches!(state, ToolState::Pending);
    let completed = matches!(state, ToolState::Completed { .. });
    let meta = match state {
        ToolState::Completed { metadata, .. } => metadata.clone(),
        _ => Value::Null,
    };
    let err = state_err(state);
    let failed = err.is_some_and(|e| !denied(e));
    let is_denied = err.is_some_and(denied);
    let expanded = ctx.expanded.contains(id.as_str());
    let color = if pending_perm {
        t.warning.base
    } else if failed {
        t.error.base
    } else {
        t.muted
    };
    let path_of = |v: &Value| s(v, "path").or_else(|| s(v, "filePath")).map(|p| ctx.path(p)).unwrap_or_default();
    // InlineTool: shows the pending copy with a spinner until the input is known
    let inline = |icon: &str, pending: &str, complete: bool, text: String, spin: bool| -> Row {
        let mut lines = if spin || (!complete && !failed) {
            ctx.inline("", if complete { &text } else { pending }, color, true, false)
        } else {
            ctx.inline(if failed { "✗" } else { icon }, &text, if failed { t.error.base } else { color }, false, is_denied)
        };
        if failed && expanded {
            let bg = t.bg;
            let avail = ctx.width.saturating_sub(3 + ICON_WIDTH).max(1);
            for l in draw::wrap(&[span(err.unwrap_or("").to_string(), t.error.base, bg)], avail, Wrap::Word) {
                let mut spans = vec![span("     ", t.text, bg)];
                spans.extend(l);
                lines.push(line(spans, bg));
            }
        }
        let r = Row::new(lines);
        if failed {
            r.click(id.clone())
        } else {
            r
        }
    };
    let n = |k: &str| meta.get(k).and_then(|v| v.as_u64());
    let count = |c: Option<u64>| c.map(|c| format!(" ({c} {})", if c == 1 { "match" } else { "matches" })).unwrap_or_default();
    match name.as_str() {
        "read" => {
            let mut text = format!("Read {}", path_of(input));
            let off = input.get("offset").and_then(|v| v.as_u64());
            let lim = input.get("limit").and_then(|v| v.as_u64());
            if off.is_some() || lim.is_some() {
                let start = off.filter(|o| *o > 0).unwrap_or(1);
                text.push_str(&format!(":{start}-{}", lim.map(|l| (start + l - 1).to_string()).unwrap_or_default()));
            }
            vec![inline("→", "Reading file…", !path_of(input).is_empty(), text, running)]
        }
        "glob" => {
            let pat = s(input, "pattern").unwrap_or("");
            let mut text = format!("Glob \"{pat}\" ");
            if let Some(p) = s(input, "path") {
                text.push_str(&format!("in {} ", ctx.path(p)));
            }
            text.push_str(count(n("count")).trim_start());
            vec![inline("✱", "Finding files…", !pat.is_empty(), text.trim_end().to_string(), false)]
        }
        "grep" => {
            let pat = s(input, "pattern").unwrap_or("");
            let mut text = format!("Grep \"{pat}\" ");
            if let Some(p) = s(input, "path") {
                text.push_str(&format!("in {} ", ctx.path(p)));
            }
            text.push_str(count(n("matches")).trim_start());
            vec![inline("✱", "Searching content…", !pat.is_empty(), text.trim_end().to_string(), false)]
        }
        "webfetch" => {
            let url = s(input, "url").unwrap_or("");
            vec![inline("%", "Fetching from the web…", !url.is_empty(), format!("WebFetch {url}"), false)]
        }
        "websearch" => {
            let q = s(input, "query").unwrap_or("");
            let label = match s(&meta, "provider") {
                Some("opencode") => "Web Search via OpenCode".to_string(),
                Some(p) => format!("Web Search via {}{}", p[..1].to_uppercase(), &p[1..]),
                None => "Web Search".into(),
            };
            vec![inline("◈", "Searching web…", !q.is_empty(), format!("{label} \"{q}\""), false)]
        }
        "skill" => {
            let n = s(&meta, "name").or_else(|| s(input, "id")).or_else(|| s(input, "name")).unwrap_or("");
            vec![inline("→", "Loading skill…", !n.is_empty(), format!("Skill \"{n}\""), false)]
        }
        "subagent" | "task" => {
            let desc = s(input, "description").unwrap_or("Subagent");
            let agent = s(input, "agent").or_else(|| s(input, "subagent_type")).unwrap_or("general");
            let sid = s(&meta, "sessionID");
            let busy = running || sid.is_some_and(|x| (ctx.child_busy)(x));
            let cont = s(input, "sessionID").is_some();
            let icon = if busy { "│" } else if cont { "↳" } else if completed { "✓" } else { "│" };
            let model = s(input, "model").map(|m| format!(" · {}", ctx.model_label(m))).unwrap_or_default();
            let text = if cont { format!("Continue subagent — {desc}{model}") } else { format!("{} Subagent — {desc}{model}", titlecase(agent)) };
            let mut r = inline(icon, "Delegating…", s(input, "description").is_some(), text, busy);
            if completed && s(&meta, "status") == Some("running") {
                badge(&mut r.lines, ctx, false);
            }
            if let Some(sid) = sid {
                r = r.click(format!("session:{sid}"));
            }
            vec![r]
        }
        "todowrite" => {
            let todos = input.get("todos").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if completed && !todos.is_empty() {
                let bg = t.raised;
                let mut items = Vec::new();
                for td in &todos {
                    let status = s(td, "status").unwrap_or("pending");
                    let content = s(td, "content").unwrap_or("");
                    let (mark, fg) = match status {
                        "completed" => ("[✓]", t.muted),
                        "in_progress" => ("[•]", t.warning.base),
                        "cancelled" => ("[-]", t.faint),
                        _ => ("[ ]", t.muted),
                    };
                    let mut st = draw::st(fg, bg);
                    if status == "cancelled" {
                        st = st.add_modifier(Modifier::CROSSED_OUT);
                    }
                    let avail = ctx.block_width().saturating_sub(4).max(1);
                    for (i, l) in draw::wrap(&[Span::styled(content.to_string(), st)], avail, Wrap::Word).into_iter().enumerate() {
                        let mut spans = vec![span(if i == 0 { format!("{mark} ") } else { "    ".into() }, fg, bg)];
                        spans.extend(l);
                        items.push(line(spans, bg));
                    }
                }
                let mut body = ctx.header("# Todos", None, if pending_perm { t.warning.base } else { t.muted }, false);
                body.extend(items);
                vec![Row::new(ctx.block(vec![body], false))]
            } else {
                vec![inline("⚙", "Updating todos…", completed || err.is_some(), if err.is_some() { "Todo update failed".into() } else { "Cleared todos".into() }, false)]
            }
        }
        "shell" | "bash" => vec![shell(id, input, state, &meta, pending_perm, expanded, ctx)],
        "write" => {
            let path = path_of(input);
            if completed {
                let content = s(input, "content").unwrap_or("");
                let mut body = ctx.header("# Wrote", Some(&path), if pending_perm { t.warning.base } else { t.muted }, false);
                let lang = highlight::lang_for_path(&path);
                let all: Vec<&str> = content.lines().collect();
                let digits = all.len().to_string().len().max(3);
                let bg = t.raised;
                for (i, src) in all.iter().enumerate() {
                    let runs = highlight::line(&src.replace('\t', "    "), &lang, &t.syntax, t.text);
                    let mut code: Vec<Span<'static>> = runs.into_iter().map(|(x, c)| span(x, c, bg)).collect();
                    let avail = ctx.block_width().saturating_sub(digits + 1).max(1);
                    let wrapped = draw::wrap(&std::mem::take(&mut code), avail, Wrap::Char);
                    for (j, l) in wrapped.into_iter().enumerate() {
                        let num = if j == 0 { format!("{:>digits$} ", i + 1) } else { " ".repeat(digits + 1) };
                        let mut spans = vec![span(num, t.muted, bg)];
                        spans.extend(l);
                        body.push(line(spans, bg));
                    }
                }
                vec![Row::new(ctx.block(vec![body, errors(err, ctx)], false))]
            } else {
                vec![inline("←", "Preparing write…", !path.is_empty(), format!("Write {path}"), false)]
            }
        }
        "edit" => {
            let files = meta.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
            let path = path_of(input);
            let hc = if pending_perm { t.warning.base } else { t.muted };
            if let Some(f) = files.first() {
                let file = s(f, "file").map(|p| ctx.path(p)).unwrap_or(path.clone());
                let patch = s(f, "patch").or_else(|| s(f, "diff")).unwrap_or("");
                let mut body = ctx.header("← Edit", Some(&file), hc, false);
                body.push(draw::blank(t.raised));
                body.extend(diff_view(patch, &file, ctx));
                vec![Row::new(ctx.block(vec![body, errors(err, ctx)], false))]
            } else if !path.is_empty() {
                vec![Row::new(ctx.block(vec![ctx.header("← Edit", Some(&path), hc, false), errors(err, ctx)], false))]
            } else {
                vec![Row::new(ctx.block(vec![ctx.header("# Preparing edit…", None, hc, streaming), errors(err, ctx)], false))]
            }
        }
        "patch" | "apply_patch" => {
            let files = meta.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
            let hc = if pending_perm { t.warning.base } else { t.muted };
            if !files.is_empty() {
                let mut rows = Vec::new();
                for (i, f) in files.iter().enumerate() {
                    let file = s(f, "file").or_else(|| s(f, "relativePath")).map(|p| ctx.path(p)).unwrap_or_default();
                    let status = s(f, "status").or_else(|| s(f, "type")).unwrap_or("modified");
                    let label = match status {
                        "added" | "add" => "# Created",
                        "deleted" | "delete" => "# Deleted",
                        _ => "← Patched",
                    };
                    let mut body = ctx.header(label, Some(&file), hc, false);
                    body.push(draw::blank(t.raised));
                    if label == "# Deleted" {
                        let d = f.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);
                        body.push(line(vec![span(format!("-{d} line{}", if d != 1 { "s" } else { "" }), t.diff.removed, t.raised)], t.raised));
                    } else {
                        body.extend(diff_view(s(f, "patch").or_else(|| s(f, "diff")).unwrap_or(""), &file, ctx));
                    }
                    let mut r = Row::new(ctx.block(vec![body], false));
                    r.tight = i > 0;
                    if i > 0 {
                        rows.push({
                            let mut g = Row::new(vec![draw::blank(t.bg)]);
                            g.tight = true;
                            g
                        });
                    }
                    rows.push(r);
                }
                rows
            } else {
                let text = s(input, "patchText").unwrap_or("");
                let targets: Vec<&str> = text
                    .lines()
                    .filter_map(|l| l.strip_prefix("*** Add File: ").or_else(|| l.strip_prefix("*** Update File: ")).or_else(|| l.strip_prefix("*** Delete File: ")))
                    .collect();
                let label = if err.is_some() { "# Patch failed" } else { "Patching" };
                let hc = if err.is_some() { t.error.base } else { hc };
                let header = if targets.len() == 1 { ctx.header(label, Some(&ctx.path(targets[0].trim())), hc, running || streaming) } else { ctx.header(label, None, hc, running || streaming) };
                let e = err.map(|e| ctx.text_lines(e, t.muted, ctx.block_width(), Wrap::Word, t.raised)).unwrap_or_default();
                vec![Row::new(ctx.block(vec![header, e], false))]
            }
        }
        _ => {
            // GenericTool: name with primitive arguments, expandable
            let args: Vec<String> = input
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| match v {
                            Value::String(x) => Some(format!("{k}={x}")),
                            Value::Number(x) => Some(format!("{k}={x}")),
                            Value::Bool(x) => Some(format!("{k}={x}")),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let summary = if args.is_empty() { name.clone() } else { format!("{name} [{}]", args.join(", ")).split_whitespace().collect::<Vec<_>>().join(" ") };
            let icon = if err.is_some() { "✗" } else { "✓" };
            let mut lines = if running || streaming { ctx.inline("", &summary, color, true, false) } else { ctx.inline(icon, &summary, color, false, is_denied) };
            if expanded {
                let bg = t.bg;
                let avail = ctx.width.saturating_sub(5).max(1);
                let mut kv = |k: &str, v: String| {
                    let kw = draw::width(k) + 2;
                    for (i, l) in draw::wrap(&[span(v, t.text, bg)], avail.saturating_sub(kw).max(1), Wrap::Word).into_iter().enumerate() {
                        let mut spans = vec![span("     ", t.text, bg), span(if i == 0 { format!("{k}: ") } else { " ".repeat(kw) }, t.muted, bg)];
                        spans.extend(l);
                        lines.push(line(spans, bg));
                    }
                };
                if let Some(o) = input.as_object() {
                    for (k, v) in o {
                        kv(k, if let Value::String(x) = v { x.clone() } else { serde_json::to_string_pretty(v).unwrap_or_default() });
                    }
                }
                let output = match state {
                    ToolState::Completed { output, .. } => output.trim().to_string(),
                    ToolState::Error { error } => error.clone(),
                    _ => String::new(),
                };
                if !output.is_empty() {
                    kv("output", output);
                }
            }
            vec![Row::new(lines).click(id.clone())]
        }
    }
}

fn errors(err: Option<&str>, ctx: &Ctx) -> Vec<Line<'static>> {
    err.map(|e| ctx.text_lines(e, ctx.t.error.base, ctx.block_width(), Wrap::Word, ctx.t.raised)).unwrap_or_default()
}

/// `StatusBadge`: " Background " appended to the last line.
fn badge(lines: &mut [Line<'static>], ctx: &Ctx, raised: bool) {
    let t = ctx.t;
    let base = if raised { t.raised } else { t.bg };
    if let Some(l) = lines.last_mut() {
        l.spans.push(span("  ", t.text, base));
        l.spans.push(span(" Background ", t.muted, t.decrease(base)));
    }
}

/// Upstream's `collapseToolOutput`.
fn collapse(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return (output.to_string(), false);
    }
    let mut visible: Vec<String> = lines.iter().take(max_lines).map(|s| s.to_string()).collect();
    if lines.len() > max_lines {
        if let Some(l) = visible.last_mut() {
            l.push('…');
        }
    }
    let preview = visible.join("\n");
    if preview.chars().count() > max_chars {
        return (preview.chars().take(max_chars.saturating_sub(1)).collect::<String>() + "…", true);
    }
    (preview, true)
}

fn collapse_tail(output: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return output.to_string();
    }
    let count = (lines.len() as isize - (max_lines as isize - 1).max(0)).max(1);
    let label = format!("({count} earlier {})", if count == 1 { "line" } else { "lines" });
    if max_lines <= 1 {
        return label;
    }
    let tail: Vec<char> = lines[lines.len().saturating_sub(max_lines - 1)..].join("\n").chars().collect();
    let avail = max_chars as isize - label.chars().count() as isize - 1;
    if avail <= 0 {
        return label;
    }
    let start = tail.len().saturating_sub(avail as usize);
    format!("{label}\n{}", tail[start..].iter().collect::<String>())
}

/// `collapseShellCommand`: at most `max_lines` visual lines of `line_width`.
fn collapse_command(input: &str, max_lines: usize, line_width: usize) -> (String, bool, usize) {
    let mut visible = String::new();
    let mut lines = 1;
    let mut w = 0;
    let mut overflow = false;
    for c in input.chars() {
        if c == '\n' {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            visible.push(c);
            lines += 1;
            w = 0;
            continue;
        }
        let cw = draw::char_width(c);
        if w + cw > line_width {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            lines += 1;
            w = 0;
        }
        visible.push(c);
        w += cw;
    }
    if !overflow {
        return (input.to_string(), false, lines);
    }
    if w >= line_width {
        visible.pop();
    }
    (visible + "…", true, lines)
}

fn collapse_shell(input: &str, output: &str, max_lines: usize, max_chars: usize) -> (String, String, bool) {
    if input.is_empty() {
        let (_, over) = collapse(output, max_lines, max_chars);
        return (String::new(), if over { collapse_tail(output, max_lines, max_chars) } else { output.to_string() }, over);
    }
    let line_chars = (max_chars / max_lines.max(1)).max(1);
    let (cmd, cover, clines) = collapse_command(input, max_lines.min(2), line_chars);
    if output.is_empty() {
        return (cmd, String::new(), cover);
    }
    let lines = max_lines.saturating_sub(clines + 1).max(1);
    let chars = max_chars.saturating_sub(cmd.chars().count() + 2).max(1);
    let (_, over) = collapse(output, lines, chars);
    (cmd, if over { collapse_tail(output, lines, chars) } else { output.to_string() }, cover || over)
}

fn strip_ansi(s: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap();
    re.replace_all(s, "").into_owned()
}

/// `ShellDisplay` inside a `BlockTool`.
fn shell(id: &str, input: &Value, state: &ToolState, meta: &Value, perm: bool, expanded: bool, ctx: &Ctx) -> Row {
    let t = ctx.t;
    let bg = t.raised;
    let running = matches!(state, ToolState::Running);
    let streaming = matches!(state, ToolState::Pending);
    let background = matches!(state, ToolState::Completed { .. }) && s(meta, "status") == Some("running");
    let command = s(input, "command").unwrap_or("");
    let workdir = s(input, "workdir").map(|w| ctx.path(w)).filter(|w| w != ".");
    let output = match state {
        ToolState::Completed { output, .. } if !background => strip_ansi(output.trim()),
        _ => String::new(),
    };
    let prefix = workdir.map(|w| format!("cd {w} && ")).unwrap_or_default();
    let input_text = if command.is_empty() { String::new() } else { format!("{}{prefix}{command}", if running { "" } else { "$ " }) };
    let max_lines = 10;
    let max_chars = max_lines * (ctx.width.saturating_sub(8 + if running { 2 } else { 0 })).max(20);
    let (cin, cout, overflow) = collapse_shell(&input_text, &output, max_lines, max_chars);
    let (shown_in, shown_out) = if expanded { (input_text.clone(), output.clone()) } else { (cin, cout) };
    let color = if perm { t.warning.base } else { t.text };
    let w = ctx.block_width();
    let mut children = Vec::new();
    let mut first = Vec::new();
    if command.is_empty() {
        let label = if running || streaming { format!("{} Writing command…", ctx.spinner) } else { "Writing command…".into() };
        first.push(line(vec![span(label, if running || streaming { color } else { t.muted }, bg)], bg));
    } else {
        let mode = if expanded { Wrap::Word } else { Wrap::Char };
        let avail = if running { w.saturating_sub(2) } else { w };
        let mut lines = ctx.text_lines(&shown_in, if running { color } else { t.text }, avail.max(1), mode, bg);
        if !expanded {
            lines.truncate(2);
        }
        if running {
            for (i, l) in lines.iter_mut().enumerate() {
                l.spans.insert(0, span(if i == 0 { format!("{} ", ctx.spinner) } else { "  ".into() }, color, bg));
            }
        }
        first.extend(lines);
        if !shown_out.is_empty() {
            first.push(line(vec![], bg));
            first.extend(ctx.text_lines(&shown_out, t.muted, w, Wrap::Word, bg));
        }
    }
    children.push(first);
    if background {
        children.push(vec![line(vec![span(" Background ", t.muted, t.decrease(bg))], bg)]);
    }
    if let ToolState::Error { error } = state {
        children.push(ctx.text_lines(&strip_ansi(error), t.error.base, w, Wrap::Word, bg));
    }
    let r = Row::new(ctx.block(children, false));
    if overflow || expanded {
        r.click(id.to_string())
    } else {
        r
    }
}

// ---------------------------------------------------------------- diffs

struct Hunk {
    header: String,
    lines: Vec<(char, String, Option<usize>, Option<usize>)>,
}

fn parse_diff(diff: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    for l in diff.lines() {
        if l.starts_with("---") || l.starts_with("+++") || l.starts_with("Index:") || l.starts_with("====") || l.starts_with("diff ") {
            continue;
        }
        if let Some(rest) = l.strip_prefix("@@") {
            let nums: Vec<usize> = rest.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
            old = nums.first().copied().unwrap_or(1);
            new = nums.get(2).copied().or(nums.get(1).copied()).unwrap_or(1);
            if nums.len() >= 4 {
                new = nums[2];
            } else if nums.len() == 3 {
                new = nums[2];
            }
            hunks.push(Hunk { header: l.to_string(), lines: vec![] });
            continue;
        }
        if hunks.is_empty() {
            hunks.push(Hunk { header: String::new(), lines: vec![] });
        }
        let h = hunks.last_mut().unwrap();
        let (sign, text) = match l.chars().next() {
            Some('+') => ('+', &l[1..]),
            Some('-') => ('-', &l[1..]),
            Some(' ') => (' ', &l[1..]),
            Some('\\') => continue,
            _ => (' ', l),
        };
        match sign {
            '+' => {
                h.lines.push(('+', text.to_string(), None, Some(new)));
                new += 1;
            }
            '-' => {
                h.lines.push(('-', text.to_string(), Some(old), None));
                old += 1;
            }
            _ => {
                h.lines.push((' ', text.to_string(), Some(old), Some(new)));
                old += 1;
                new += 1;
            }
        }
    }
    hunks.retain(|h| !h.lines.is_empty());
    hunks
}

/// Unified or split diff, like opentui's `<diff>` with line numbers.
fn diff_view(diff: &str, path: &str, ctx: &Ctx) -> Vec<Line<'static>> {
    let t = ctx.t;
    let d = &t.diff;
    let hunks = parse_diff(diff);
    if hunks.is_empty() {
        return vec![line(vec![span("No changes", t.muted, t.raised)], t.raised)];
    }
    let lang = highlight::lang_for_path(path);
    let max_num = hunks.iter().flat_map(|h| h.lines.iter().flat_map(|l| [l.2, l.3])).flatten().max().unwrap_or(1);
    let digits = max_num.to_string().len();
    // the diff box has one column of padding on each side
    let total = ctx.block_width().saturating_sub(2).max(10);
    let split = ctx.width > 120;
    let row = |sign: char, text: &str, num: Option<usize>, width: usize| -> Vec<Line<'static>> {
        let (bg, nbg, sc) = match sign {
            '+' => (d.bg_added, d.line_number_bg_added, d.highlight_added),
            '-' => (d.bg_removed, d.line_number_bg_removed, d.highlight_removed),
            _ => (d.bg_context, d.bg_context, d.context),
        };
        let gutter = digits + 2;
        let avail = width.saturating_sub(gutter + 2).max(1);
        let runs = highlight::line(&text.replace('\t', "    "), &lang, &t.syntax, t.text);
        let code: Vec<Span<'static>> = runs.into_iter().map(|(x, c)| span(x, c, bg)).collect();
        let wrapped = draw::wrap(&code, avail, Wrap::Word);
        wrapped
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let n = if i == 0 { num.map(|n| format!(" {n:>digits$} ")).unwrap_or_else(|| " ".repeat(gutter)) } else { " ".repeat(gutter) };
                let mut spans = vec![span(n, d.line_number, nbg), span(if i == 0 && sign != ' ' { format!("{sign} ") } else { "  ".into() }, sc, bg)];
                spans.extend(l);
                let used: usize = spans.iter().map(|s| draw::width(&s.content)).sum();
                spans.push(span(" ".repeat(width.saturating_sub(used)), t.text, bg));
                Line::from(spans)
            })
            .collect()
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    let pad = span(" ", t.text, t.raised);
    for (hi, h) in hunks.iter().enumerate() {
        if hi > 0 {
            let mut l = vec![span(format!(" {}", h.header), d.hunk_header, d.bg_context)];
            let used = draw::width(&h.header) + 1;
            l.push(span(" ".repeat(total.saturating_sub(used)), t.text, d.bg_context));
            out.push(Line::from(l));
        }
        if !split {
            for (sign, text, o, n) in &h.lines {
                out.extend(row(*sign, text, if *sign == '-' { *o } else { *n }, total));
            }
            continue;
        }
        let half = total / 2;
        let mut i = 0;
        while i < h.lines.len() {
            let (sign, ..) = &h.lines[i];
            if *sign == ' ' {
                let (_, text, o, n) = &h.lines[i];
                zip(&mut out, row(' ', text, *o, half), row(' ', text, *n, total - half), half, total - half, t);
                i += 1;
                continue;
            }
            let mut dels = Vec::new();
            let mut adds = Vec::new();
            while i < h.lines.len() && h.lines[i].0 == '-' {
                dels.push(&h.lines[i]);
                i += 1;
            }
            while i < h.lines.len() && h.lines[i].0 == '+' {
                adds.push(&h.lines[i]);
                i += 1;
            }
            for k in 0..dels.len().max(adds.len()) {
                let left = dels.get(k).map(|l| row('-', &l.1, l.2, half)).unwrap_or_default();
                let right = adds.get(k).map(|l| row('+', &l.1, l.3, total - half)).unwrap_or_default();
                zip(&mut out, left, right, half, total - half, t);
            }
        }
    }
    out.into_iter()
        .map(|mut l| {
            let mut spans = vec![pad.clone()];
            spans.append(&mut l.spans);
            line(spans, t.raised)
        })
        .collect()
}

fn zip(out: &mut Vec<Line<'static>>, left: Vec<Line<'static>>, right: Vec<Line<'static>>, lw: usize, rw: usize, t: &Theme) {
    let n = left.len().max(right.len());
    let empty = |w: usize| Line::from(vec![span(" ".repeat(w), t.text, t.diff.bg_context)]);
    for i in 0..n {
        let mut l = left.get(i).cloned().unwrap_or_else(|| empty(lw));
        let r = right.get(i).cloned().unwrap_or_else(|| empty(rw));
        l.spans.extend(r.spans);
        out.push(l);
    }
}
