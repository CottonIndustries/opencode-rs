//! Conversation rendering: markdown to styled, wrapped lines, plus tool call display.

use crate::session::{Message, Part, Role, ToolState};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

pub struct Theme {
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub user: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub code: Color,
    pub heading: Color,
}

pub const THEME: Theme = Theme {
    text: Color::Reset,
    muted: Color::DarkGray,
    accent: Color::Rgb(250, 178, 131),
    user: Color::Rgb(92, 156, 245),
    success: Color::Rgb(127, 216, 143),
    error: Color::Rgb(224, 108, 117),
    warning: Color::Rgb(229, 192, 123),
    code: Color::Rgb(152, 195, 121),
    heading: Color::Rgb(157, 124, 216),
};

/// Word-wrap a styled line to `width`, prefixing continuation lines with `indent`.
pub fn wrap(spans: Vec<Span<'static>>, width: usize, first_prefix: Vec<Span<'static>>, prefix: Vec<Span<'static>>) -> Vec<Line<'static>> {
    let width = width.max(10);
    let pw = |p: &Vec<Span>| p.iter().map(|s| s.content.width()).sum::<usize>();
    let mut out = Vec::new();
    let mut cur: Vec<Span<'static>> = first_prefix.clone();
    let mut cur_w = pw(&first_prefix);
    let cont_w = pw(&prefix);
    let mut line_has_content = false;
    // split spans into tokens of (text, style), keeping whitespace tokens
    let mut tokens: Vec<(String, Style)> = Vec::new();
    for s in spans {
        let mut buf = String::new();
        let mut in_space = None;
        for ch in s.content.chars() {
            let sp = ch == ' ';
            if in_space.is_some_and(|w| w != sp) {
                tokens.push((std::mem::take(&mut buf), s.style));
            }
            in_space = Some(sp);
            buf.push(ch);
        }
        if !buf.is_empty() {
            tokens.push((buf, s.style));
        }
    }
    for (tok, style) in tokens {
        let w = tok.width();
        let is_space = tok.chars().all(|c| c == ' ');
        if cur_w + w > width && line_has_content {
            out.push(Line::from(std::mem::take(&mut cur)));
            cur = prefix.clone();
            cur_w = cont_w;
            line_has_content = false;
            if is_space {
                continue;
            }
        }
        if w > width.saturating_sub(cur_w) && !is_space {
            // hard-break long tokens
            let mut chunk = String::new();
            let mut cw = 0;
            for ch in tok.chars() {
                let chw = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]) as &str);
                if cur_w + cw + chw > width && (cw > 0 || line_has_content) {
                    cur.push(Span::styled(std::mem::take(&mut chunk), style));
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur = prefix.clone();
                    cur_w = cont_w;
                    cw = 0;
                    line_has_content = false;
                }
                chunk.push(ch);
                cw += chw;
            }
            if !chunk.is_empty() {
                cur.push(Span::styled(chunk, style));
                cur_w += cw;
                line_has_content = true;
            }
            continue;
        }
        cur.push(Span::styled(tok, style));
        cur_w += w;
        line_has_content = true;
    }
    out.push(Line::from(cur));
    out
}

/// Render markdown to wrapped lines.
pub fn markdown(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut styles: Vec<Style> = vec![base];
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut pending_prefix: Option<Vec<Span<'static>>> = None;
    let mut quote = 0usize;
    let mut code: Option<String> = None;
    let mut code_lang = String::new();
    let cur = |styles: &Vec<Style>| *styles.last().unwrap();

    let indent = |list_stack: &Vec<Option<u64>>, quote: usize| -> Vec<Span<'static>> {
        let mut p = Vec::new();
        if quote > 0 {
            p.push(Span::styled("│ ".repeat(quote), Style::default().fg(THEME.muted)));
        }
        if !list_stack.is_empty() {
            p.push(Span::raw("  ".repeat(list_stack.len())));
        }
        p
    };
    let flush = |out: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>, pending: &mut Option<Vec<Span<'static>>>, list_stack: &Vec<Option<u64>>, quote: usize| {
        if spans.is_empty() && pending.is_none() {
            return;
        }
        let cont = indent(list_stack, quote);
        let first = pending.take().unwrap_or_else(|| cont.clone());
        out.extend(wrap(std::mem::take(spans), width, first, cont));
    };
    let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS);
    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    if !out.is_empty() {
                        out.push(Line::default());
                    }
                    let mut s = base.fg(THEME.heading).add_modifier(Modifier::BOLD);
                    if level == HeadingLevel::H1 {
                        s = s.add_modifier(Modifier::UNDERLINED);
                    }
                    styles.push(s);
                }
                Tag::Paragraph => {}
                Tag::BlockQuote(_) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    quote += 1;
                    styles.push(cur(&styles).fg(THEME.muted).add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    code = Some(String::new());
                    code_lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::List(start) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    let depth = list_stack.len().saturating_sub(1);
                    let marker = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => "• ".to_string(),
                    };
                    let mut p = Vec::new();
                    if quote > 0 {
                        p.push(Span::styled("│ ".repeat(quote), Style::default().fg(THEME.muted)));
                    }
                    p.push(Span::raw("  ".repeat(depth)));
                    p.push(Span::styled(marker, Style::default().fg(THEME.accent)));
                    pending_prefix = Some(p);
                }
                Tag::Emphasis => styles.push(cur(&styles).add_modifier(Modifier::ITALIC)),
                Tag::Strong => styles.push(cur(&styles).add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => styles.push(cur(&styles).add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { .. } => styles.push(cur(&styles).fg(THEME.user).add_modifier(Modifier::UNDERLINED)),
                Tag::Table(_) | Tag::TableHead | Tag::TableRow => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                }
                Tag::TableCell => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    styles.pop();
                }
                TagEnd::Paragraph => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    if list_stack.is_empty() {
                        out.push(Line::default());
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    quote = quote.saturating_sub(1);
                    styles.pop();
                }
                TagEnd::CodeBlock => {
                    let body = code.take().unwrap_or_default();
                    let pad = indent(&list_stack, quote);
                    if !code_lang.is_empty() {
                        let mut l = pad.clone();
                        l.push(Span::styled(format!("  {code_lang}"), Style::default().fg(THEME.muted)));
                        out.push(Line::from(l));
                    }
                    for line in body.trim_end_matches('\n').split('\n') {
                        let mut l = pad.clone();
                        l.push(Span::styled("  ", Style::default()));
                        let (style, text) = if code_lang == "diff" {
                            let s = if line.starts_with('+') {
                                Style::default().fg(THEME.success)
                            } else if line.starts_with('-') {
                                Style::default().fg(THEME.error)
                            } else {
                                Style::default().fg(THEME.code)
                            };
                            (s, line)
                        } else {
                            (Style::default().fg(THEME.code), line)
                        };
                        let expanded = text.replace('\t', "    ");
                        for (i, w) in wrap(vec![Span::styled(expanded, style)], width, l.clone(), { let mut c = pad.clone(); c.push(Span::raw("  ")); c }).into_iter().enumerate() {
                            let _ = i;
                            out.push(w);
                        }
                    }
                    out.push(Line::default());
                }
                TagEnd::List(_) => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    list_stack.pop();
                    if list_stack.is_empty() {
                        out.push(Line::default());
                    }
                }
                TagEnd::Item => flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    styles.pop();
                }
                TagEnd::TableCell => spans.push(Span::styled(" │ ", Style::default().fg(THEME.muted))),
                TagEnd::TableHead | TagEnd::TableRow => flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote),
                TagEnd::Table => {
                    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                    out.push(Line::default());
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(c) = code.as_mut() {
                    c.push_str(&t);
                } else {
                    spans.push(Span::styled(t.to_string(), cur(&styles)));
                }
            }
            Event::Code(t) => spans.push(Span::styled(t.to_string(), cur(&styles).fg(THEME.code))),
            Event::SoftBreak => spans.push(Span::styled(" ", cur(&styles))),
            Event::HardBreak => flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote),
            Event::Rule => {
                flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
                out.push(Line::from(Span::styled("─".repeat(width.min(80)), Style::default().fg(THEME.muted))));
            }
            Event::TaskListMarker(done) => spans.push(Span::styled(if done { "[x] " } else { "[ ] " }, Style::default().fg(THEME.accent))),
            Event::Html(h) | Event::InlineHtml(h) => spans.push(Span::styled(h.to_string(), cur(&styles).fg(THEME.muted))),
            _ => {}
        }
    }
    flush(&mut out, &mut spans, &mut pending_prefix, &list_stack, quote);
    while out.last().is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty())) {
        out.pop();
    }
    out
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// One-line summary of a tool call's input.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let path = || {
        let p = s(input, "path");
        if p.is_empty() { s(input, "filePath") } else { p }
    };
    match name {
        "shell" => {
            let c = s(input, "command");
            let mut out = crate::util::first_line(&c).to_string();
            if c.lines().count() > 1 {
                out.push_str(" …");
            }
            if input.get("background").and_then(|b| b.as_bool()) == Some(true) {
                out.push_str("  (background)");
            }
            out
        }
        "read" | "write" | "edit" => {
            let mut p = path();
            if let Some(o) = input.get("offset").and_then(|x| x.as_u64()) {
                p.push_str(&format!(" [offset={o}]"));
            }
            p
        }
        "glob" => format!("{}{}", s(input, "pattern"), if s(input, "path").is_empty() { String::new() } else { format!(" in {}", s(input, "path")) }),
        "grep" => format!("\"{}\"{}", s(input, "pattern"), if s(input, "include").is_empty() { String::new() } else { format!(" ({})", s(input, "include")) }),
        "webfetch" => s(input, "url"),
        "websearch" => s(input, "query"),
        "subagent" => format!("@{} {}", s(input, "agent"), s(input, "description")),
        "skill" => s(input, "id"),
        "todowrite" => format!("{} todos", input.get("todos").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0)),
        "patch" => {
            let t = s(input, "patchText");
            let files: Vec<&str> = t
                .lines()
                .filter_map(|l| l.strip_prefix("*** Add File: ").or_else(|| l.strip_prefix("*** Update File: ")).or_else(|| l.strip_prefix("*** Delete File: ")))
                .collect();
            files.join(", ")
        }
        _ => {
            let raw = input.to_string();
            crate::util::truncate_chars(&raw, 120)
        }
    }
}

pub struct RenderOpts {
    pub width: usize,
    pub thinking: bool,
    pub details: bool,
    pub spinner: &'static str,
}

fn diff_lines(diff: &str, width: usize, max: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let body: Vec<&str> = diff.lines().filter(|l| !l.starts_with("---") && !l.starts_with("+++")).collect();
    for l in body.iter().take(max) {
        let style = if l.starts_with('+') {
            Style::default().fg(THEME.success)
        } else if l.starts_with('-') {
            Style::default().fg(THEME.error)
        } else {
            Style::default().fg(THEME.muted)
        };
        out.push(Line::from(vec![Span::raw("    "), Span::styled(crate::util::truncate_chars(&l.replace('\t', "    "), width.saturating_sub(6)), style)]));
    }
    if body.len() > max {
        out.push(Line::from(Span::styled(format!("    … {} more lines", body.len() - max), Style::default().fg(THEME.muted))));
    }
    out
}

fn tool_lines(name: &str, input: &Value, state: &ToolState, o: &RenderOpts) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let (icon, color) = match state {
        ToolState::Pending => ("○", THEME.muted),
        ToolState::Running => (o.spinner, THEME.accent),
        ToolState::Completed { .. } => ("✓", THEME.success),
        ToolState::Error { .. } => ("✗", THEME.error),
    };
    let summary = tool_summary(name, input);
    out.extend(wrap(
        vec![Span::styled(summary, Style::default().fg(THEME.text))],
        o.width,
        vec![Span::styled(format!("  {icon} "), Style::default().fg(color)), Span::styled(format!("{name} "), Style::default().fg(color).add_modifier(Modifier::BOLD))],
        vec![Span::raw("      ")],
    ));
    match state {
        ToolState::Completed { output, metadata, .. } => {
            if let Some(files) = metadata.get("files").and_then(|f| f.as_array()) {
                for f in files {
                    let add = f.get("additions").and_then(|x| x.as_u64()).unwrap_or(0);
                    let del = f.get("deletions").and_then(|x| x.as_u64()).unwrap_or(0);
                    out.push(Line::from(vec![
                        Span::styled(format!("    {} ", s(f, "file")), Style::default().fg(THEME.muted)),
                        Span::styled(format!("+{add}"), Style::default().fg(THEME.success)),
                        Span::raw(" "),
                        Span::styled(format!("-{del}"), Style::default().fg(THEME.error)),
                    ]));
                    let diff = s(f, "diff");
                    if !diff.is_empty() {
                        out.extend(diff_lines(&diff, o.width, if o.details { 200 } else { 24 }));
                    }
                }
            } else if name == "todowrite" {
                for t in metadata.get("todos").and_then(|t| t.as_array()).into_iter().flatten() {
                    let st = s(t, "status");
                    let (mark, style) = match st.as_str() {
                        "completed" => ("[✓]", Style::default().fg(THEME.success).add_modifier(Modifier::CROSSED_OUT)),
                        "in_progress" => ("[•]", Style::default().fg(THEME.accent)),
                        "cancelled" => ("[-]", Style::default().fg(THEME.muted).add_modifier(Modifier::CROSSED_OUT)),
                        _ => ("[ ]", Style::default()),
                    };
                    out.push(Line::from(vec![Span::raw("    "), Span::styled(format!("{mark} {}", s(t, "content")), style)]));
                }
            } else if o.details || name == "shell" {
                let max = if o.details { 40 } else { 6 };
                let lines: Vec<&str> = output.lines().collect();
                let skip = if name == "shell" && !o.details { lines.len().saturating_sub(max) } else { 0 };
                if skip > 0 {
                    out.push(Line::from(Span::styled(format!("    … {skip} earlier lines"), Style::default().fg(THEME.muted))));
                }
                for l in lines.iter().skip(skip).take(max) {
                    out.push(Line::from(vec![Span::raw("    "), Span::styled(crate::util::truncate_chars(&l.replace('\t', "    "), o.width.saturating_sub(6)), Style::default().fg(THEME.muted))]));
                }
                if skip == 0 && lines.len() > max {
                    out.push(Line::from(Span::styled(format!("    … {} more lines", lines.len() - max), Style::default().fg(THEME.muted))));
                }
            }
        }
        ToolState::Error { error } => {
            for l in error.lines().take(if o.details { 20 } else { 3 }) {
                out.extend(wrap(vec![Span::styled(l.to_string(), Style::default().fg(THEME.error))], o.width, vec![Span::raw("    ")], vec![Span::raw("    ")]));
            }
        }
        _ => {}
    }
    out
}

pub fn message(m: &Message, o: &RenderOpts) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    match m.role {
        Role::User => {
            let bar = || vec![Span::styled("┃ ", Style::default().fg(THEME.user))];
            let mut any = false;
            for p in &m.parts {
                match p {
                    Part::Text { text, synthetic } => {
                        if *synthetic && !o.details {
                            let first = crate::util::first_line(text.trim());
                            if first.starts_with("<shell") || first.starts_with("<subagent") {
                                out.push(Line::from(vec![Span::styled("┃ ", Style::default().fg(THEME.muted)), Span::styled(crate::util::truncate_chars(first, o.width.saturating_sub(4)), Style::default().fg(THEME.muted))]));
                                any = true;
                            }
                            continue;
                        }
                        for l in text.trim_end().split('\n') {
                            let style = if *synthetic { Style::default().fg(THEME.muted) } else { Style::default() };
                            out.extend(wrap(vec![Span::styled(l.to_string(), style)], o.width, bar(), bar()));
                        }
                        any = true;
                    }
                    Part::File(f) => {
                        out.push(Line::from(vec![Span::styled("┃ ", Style::default().fg(THEME.user)), Span::styled(format!("📎 {} ({})", f.name, f.mime), Style::default().fg(THEME.muted))]));
                        any = true;
                    }
                    Part::Compaction { summary } => {
                        out.push(Line::from(Span::styled("── conversation compacted ──", Style::default().fg(THEME.muted))));
                        if o.details {
                            out.extend(markdown(summary, o.width, Style::default().fg(THEME.muted)));
                        }
                        any = true;
                    }
                    _ => {}
                }
            }
            if any {
                out.push(Line::default());
            }
        }
        Role::Assistant => {
            for p in &m.parts {
                match p {
                    Part::Reasoning { text, .. } if o.thinking && !text.trim().is_empty() => {
                        let style = Style::default().fg(THEME.muted).add_modifier(Modifier::ITALIC);
                        let mut lines = markdown(text, o.width.saturating_sub(2), style);
                        if let Some(first) = lines.first_mut() {
                            first.spans.insert(0, Span::styled("Thinking: ", style.add_modifier(Modifier::BOLD)));
                        }
                        out.extend(lines.into_iter().map(|mut l| {
                            l.spans.insert(0, Span::styled("  ", style));
                            l
                        }));
                        out.push(Line::default());
                    }
                    Part::Text { text, .. } if !text.trim().is_empty() => {
                        if out.last().is_some_and(|l: &Line| !l.spans.is_empty()) {
                            out.push(Line::default());
                        }
                        out.extend(markdown(text, o.width, Style::default()));
                        out.push(Line::default());
                    }
                    Part::Tool { name, input, state, .. } => {
                        out.extend(tool_lines(name, input, state, o));
                    }
                    _ => {}
                }
            }
            if m.parts.iter().any(|p| matches!(p, Part::Tool { .. })) && !out.last().is_some_and(|l| l.spans.is_empty()) {
                out.push(Line::default());
            }
            if let Some(e) = &m.error {
                out.extend(wrap(vec![Span::styled(e.clone(), Style::default().fg(THEME.error))], o.width, vec![Span::styled("  ! ", Style::default().fg(THEME.error))], vec![Span::raw("    ")]));
                out.push(Line::default());
            }
        }
    }
    out
}
