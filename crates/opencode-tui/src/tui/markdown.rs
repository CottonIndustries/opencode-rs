//! Markdown to styled lines, in the style of opentui's `<markdown>` with `conceal`:
//! markers are hidden, theme `markdown.*` tokens color each element, code is highlighted.

use super::draw::{self, line, Wrap};
use super::highlight;
use super::theme::{Rgba, Tokens};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Copy)]
struct Inline {
    fg: Rgba,
    mods: Modifier,
}

struct Ctx<'a> {
    t: &'a Tokens,
    bg: Rgba,
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    stack: Vec<Inline>,
    /// (next ordinal, continuation indent) per open list.
    lists: Vec<(Option<u64>, usize)>,
    quote: usize,
    first_prefix: Option<Vec<Span<'static>>>,
    code: Option<(String, String)>,
    table: Option<Table>,
    link: Vec<String>,
}

struct Table {
    align: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header: usize,
    cell: Vec<Span<'static>>,
    row: Vec<Vec<Span<'static>>>,
}

impl Ctx<'_> {
    fn style(&self, i: Inline) -> Style {
        draw::st(i.fg, self.bg).add_modifier(i.mods)
    }
    fn cur(&self) -> Inline {
        *self.stack.last().unwrap()
    }
    fn push_text(&mut self, text: &str) {
        let s = self.style(self.cur());
        if let Some(t) = self.table.as_mut() {
            t.cell.push(Span::styled(text.to_string(), s));
        } else {
            self.spans.push(Span::styled(text.to_string(), s));
        }
    }
    fn prefix(&self) -> Vec<Span<'static>> {
        let mut p = Vec::new();
        if self.quote > 0 {
            p.push(draw::span("│ ".repeat(self.quote), self.t.markdown.block_quote, self.bg));
        }
        let indent: usize = self.lists.iter().map(|(_, w)| *w).sum();
        if indent > 0 {
            p.push(draw::span(" ".repeat(indent), self.t.text, self.bg));
        }
        p
    }
    fn gap(&mut self) {
        if self.out.last().is_some_and(|l| !l.spans.is_empty()) {
            let p = if self.quote > 0 { vec![draw::span("│ ".repeat(self.quote), self.t.markdown.block_quote, self.bg)] } else { vec![] };
            self.out.push(line(p, self.bg));
        }
    }
    fn flush(&mut self) {
        if self.spans.is_empty() && self.first_prefix.is_none() {
            return;
        }
        let cont = self.prefix();
        let first = self.first_prefix.take().unwrap_or_else(|| cont.clone());
        let pw = draw::line_width(&Line::from(first.clone())).max(draw::line_width(&Line::from(cont.clone())));
        let spans = std::mem::take(&mut self.spans);
        for (i, l) in draw::wrap(&spans, self.width.saturating_sub(pw).max(1), Wrap::Word).into_iter().enumerate() {
            let mut s = if i == 0 { first.clone() } else { cont.clone() };
            s.extend(l);
            self.out.push(line(s, self.bg));
        }
    }
}

pub fn render(text: &str, width: usize, t: &Tokens, bg: Rgba) -> Vec<Line<'static>> {
    render_with(text, width, t, bg, t.markdown.text)
}

/// Render with a base text color (thinking blocks use the muted color throughout).
pub fn render_with(text: &str, width: usize, t: &Tokens, bg: Rgba, base: Rgba) -> Vec<Line<'static>> {
    let mono = base != t.markdown.text;
    let mut c = Ctx {
        t,
        bg,
        width: width.max(4),
        out: Vec::new(),
        spans: Vec::new(),
        stack: vec![Inline { fg: base, mods: Modifier::empty() }],
        lists: Vec::new(),
        quote: 0,
        first_prefix: None,
        code: None,
        table: None,
        link: Vec::new(),
    };
    let pick = |color: Rgba| if mono { base } else { color };
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(text, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if c.first_prefix.is_none() && c.lists.is_empty() {
                        c.gap();
                    }
                }
                Tag::Heading { level, .. } => {
                    c.flush();
                    c.gap();
                    let mut mods = Modifier::BOLD;
                    if level == HeadingLevel::H1 {
                        mods |= Modifier::UNDERLINED;
                    }
                    c.stack.push(Inline { fg: pick(t.markdown.heading), mods });
                }
                Tag::BlockQuote(_) => {
                    c.flush();
                    c.gap();
                    c.quote += 1;
                    let cur = c.cur();
                    c.stack.push(Inline { fg: pick(t.markdown.block_quote), mods: cur.mods | Modifier::ITALIC });
                }
                Tag::CodeBlock(kind) => {
                    c.flush();
                    c.gap();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    c.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    c.flush();
                    if c.lists.is_empty() {
                        c.gap();
                    }
                    c.lists.push((start, 2));
                }
                Tag::Item => {
                    c.flush();
                    let outer: usize = c.lists.iter().rev().skip(1).map(|(_, w)| *w).sum();
                    let (marker, color) = match c.lists.last_mut() {
                        Some((Some(n), w)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            *w = m.len();
                            (m, t.markdown.list_enumeration)
                        }
                        Some((None, w)) => {
                            *w = 2;
                            ("- ".to_string(), t.markdown.list_item)
                        }
                        None => ("- ".to_string(), t.markdown.list_item),
                    };
                    let mut p = Vec::new();
                    if c.quote > 0 {
                        p.push(draw::span("│ ".repeat(c.quote), t.markdown.block_quote, bg));
                    }
                    p.push(draw::span(" ".repeat(outer), t.text, bg));
                    p.push(draw::span(marker, pick(color), bg));
                    c.first_prefix = Some(p);
                }
                Tag::Emphasis => {
                    let cur = c.cur();
                    c.stack.push(Inline { fg: pick(t.markdown.emphasis), mods: cur.mods | Modifier::ITALIC });
                }
                Tag::Strong => {
                    let cur = c.cur();
                    c.stack.push(Inline { fg: pick(t.markdown.strong), mods: cur.mods | Modifier::BOLD });
                }
                Tag::Strikethrough => {
                    let cur = c.cur();
                    c.stack.push(Inline { fg: cur.fg, mods: cur.mods | Modifier::CROSSED_OUT });
                }
                Tag::Link { dest_url, .. } => {
                    let cur = c.cur();
                    c.link.push(dest_url.to_string());
                    c.stack.push(Inline { fg: pick(t.markdown.link_text), mods: cur.mods | Modifier::UNDERLINED });
                }
                Tag::Image { dest_url, .. } => {
                    let cur = c.cur();
                    c.link.push(dest_url.to_string());
                    c.stack.push(Inline { fg: pick(t.markdown.image_text), mods: cur.mods });
                }
                Tag::Table(align) => {
                    c.flush();
                    c.gap();
                    c.table = Some(Table { align, rows: vec![], header: 0, cell: vec![], row: vec![] });
                }
                Tag::TableHead => {
                    let cur = c.cur();
                    c.stack.push(Inline { fg: cur.fg, mods: cur.mods | Modifier::BOLD });
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => c.flush(),
                TagEnd::Heading(_) => {
                    c.flush();
                    c.stack.pop();
                }
                TagEnd::BlockQuote(_) => {
                    c.flush();
                    c.quote = c.quote.saturating_sub(1);
                    c.stack.pop();
                }
                TagEnd::CodeBlock => {
                    let (lang, body) = c.code.take().unwrap_or_default();
                    let prefix = c.prefix();
                    let pw = draw::line_width(&Line::from(prefix.clone()));
                    let source = body.trim_end_matches('\n').replace('\t', "    ");
                    for runs in highlight::text(&source, &lang, &t.syntax, pick(t.markdown.code_block)) {
                        let spans: Vec<Span<'static>> = runs.into_iter().map(|(s, col)| draw::span(s, if mono { base } else { col }, bg)).collect();
                        for l in draw::wrap(&spans, c.width.saturating_sub(pw).max(1), Wrap::Char) {
                            let mut s = prefix.clone();
                            s.extend(l);
                            c.out.push(line(s, bg));
                        }
                    }
                }
                TagEnd::List(_) => {
                    c.flush();
                    c.lists.pop();
                }
                TagEnd::Item => c.flush(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    c.stack.pop();
                }
                TagEnd::Link | TagEnd::Image => {
                    c.stack.pop();
                    if let Some(url) = c.link.pop() {
                        let shown: String = c.spans.iter().rev().take(1).map(|s| s.content.to_string()).collect();
                        if !url.is_empty() && shown != url && !url.starts_with('#') {
                            let s = draw::st(pick(t.markdown.link), bg);
                            let target = if let Some(tb) = c.table.as_mut() { &mut tb.cell } else { &mut c.spans };
                            target.push(Span::styled(format!(" ({url})"), s));
                        }
                    }
                }
                TagEnd::TableHead => {
                    c.stack.pop();
                    if let Some(tb) = c.table.as_mut() {
                        let row = std::mem::take(&mut tb.row);
                        tb.rows.push(row);
                        tb.header = tb.rows.len();
                    }
                }
                TagEnd::TableCell => {
                    if let Some(tb) = c.table.as_mut() {
                        let cell = std::mem::take(&mut tb.cell);
                        tb.row.push(cell);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(tb) = c.table.as_mut() {
                        let row = std::mem::take(&mut tb.row);
                        tb.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(tb) = c.table.take() {
                        let lines = table(&tb, c.width, t, bg);
                        c.out.extend(lines);
                    }
                }
                _ => {}
            },
            Event::Text(s) => {
                if let Some((_, body)) = c.code.as_mut() {
                    body.push_str(&s);
                } else {
                    c.push_text(&s);
                }
            }
            Event::Code(s) => {
                let cur = c.cur();
                let st = c.style(Inline { fg: pick(t.markdown.code), mods: cur.mods });
                match c.table.as_mut() {
                    Some(tb) => tb.cell.push(Span::styled(s.to_string(), st)),
                    None => c.spans.push(Span::styled(s.to_string(), st)),
                }
            }
            Event::SoftBreak => c.push_text(" "),
            Event::HardBreak => c.flush(),
            Event::Rule => {
                c.flush();
                c.gap();
                c.out.push(line(vec![draw::span("─".repeat(c.width), pick(t.markdown.horizontal_rule), bg)], bg));
            }
            Event::TaskListMarker(done) => {
                let st = draw::st(pick(t.markdown.list_item), bg);
                c.spans.push(Span::styled(if done { "[x] " } else { "[ ] " }, st));
            }
            Event::Html(h) | Event::InlineHtml(h) => c.push_text(&h),
            _ => {}
        }
    }
    c.flush();
    while c.out.last().is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty())) {
        c.out.pop();
    }
    c.out
}

fn table(tb: &Table, width: usize, t: &Tokens, bg: Rgba) -> Vec<Line<'static>> {
    let cols = tb.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return vec![];
    }
    let cell_w = |c: &Vec<Span>| draw::line_width(&Line::from(c.clone()));
    let mut widths = vec![1usize; cols];
    for r in &tb.rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell_w(c));
        }
    }
    // shrink the widest columns until the grid fits
    let total = |w: &Vec<usize>| w.iter().sum::<usize>() + cols * 3 + 1;
    while total(&widths) > width {
        let (i, m) = widths.iter().enumerate().max_by_key(|(_, w)| **w).map(|(i, w)| (i, *w)).unwrap();
        if m <= 3 {
            break;
        }
        widths[i] = m - 1;
    }
    let border = |s: String| draw::span(s, t.border, bg);
    let rule = |l: &str, m: &str, r: &str| {
        let mut s = l.to_string();
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 == cols { r } else { m });
        }
        line(vec![border(s)], bg)
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    for (ri, r) in tb.rows.iter().enumerate() {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..cols).map(|i| r.get(i).map(|c| draw::wrap(c, widths[i], Wrap::Word)).unwrap_or_else(|| vec![vec![]])).collect();
        let h = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        for li in 0..h {
            let mut spans = vec![border("│".into())];
            for i in 0..cols {
                let content = wrapped[i].get(li).cloned().unwrap_or_default();
                let cw = draw::line_width(&Line::from(content.clone()));
                let pad = widths[i].saturating_sub(cw);
                let (left, right) = match tb.align.get(i) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                spans.push(draw::span(" ".repeat(1 + left), t.text, bg));
                spans.extend(content);
                spans.push(draw::span(" ".repeat(1 + right), t.text, bg));
                spans.push(border("│".into()));
            }
            out.push(line(spans, bg));
        }
        if ri + 1 < tb.rows.len() {
            out.push(if ri + 1 == tb.header { rule("├", "┼", "┤") } else { rule("├", "┼", "┤") });
        }
    }
    out.push(rule("└", "┴", "┘"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect()
    }

    #[test]
    fn conceals_markers() {
        let t = super::super::theme::fallback(super::super::theme::Mode::Dark);
        let out = plain(&render("# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n\n```rust\nfn main() {}\n```", 60, &t, t.bg));
        assert_eq!(out, vec!["Title", "", "Some bold and code.", "", "- one", "- two", "", "fn main() {}"]);
    }

    #[test]
    fn tables_are_grids() {
        let t = super::super::theme::fallback(super::super::theme::Mode::Dark);
        let out = plain(&render("| a | b |\n|---|---|\n| 1 | 22 |", 40, &t, t.bg));
        assert_eq!(out[0], "┌───┬────┐");
        assert_eq!(out[1], "│ a │ b  │");
        assert_eq!(out[3], "│ 1 │ 22 │");
    }
}
