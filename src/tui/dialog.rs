//! Dialogs in upstream's classic design (`ui/dialog.tsx`, `ui/dialog-select.tsx`): a panel a
//! quarter down the screen over a dimmed backdrop, with a title row, search field, grouped list
//! and an action footer.

use super::draw::{self, line, span};
use super::prompt::{fuzzy, Editor};
use super::theme::{Theme, Tokens};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

pub const INSET: u16 = 3;
pub const LIST_INSET: u16 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    Medium,
    Large,
}

pub fn width(size: Size) -> u16 {
    match size {
        Size::Large => 88,
        Size::Medium => 60,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Palette,
    Models,
    Sessions,
    Agents,
    Variants,
    Themes,
    Timeline,
    Settings,
    Mcp,
    Fork,
}

#[derive(Clone, Debug, Default)]
pub struct Opt {
    pub title: String,
    pub description: String,
    pub footer: String,
    pub category: String,
    pub value: String,
    pub search: String,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub struct Hint {
    pub title: String,
    pub key: String,
    pub right: bool,
}

pub struct Select {
    pub kind: Kind,
    pub title: String,
    pub suffix: String,
    pub placeholder: String,
    pub options: Vec<Opt>,
    pub filter: Editor,
    pub selected: usize,
    pub scroll: usize,
    /// Drop categories while filtering (palette, models).
    pub flat: bool,
    pub size: Size,
    pub hints: Vec<Hint>,
    pub confirm: Option<String>,
    pub all: bool,
    /// Options shown only while unfiltered (palette "Suggested").
    pub pinned: usize,
}

impl Select {
    pub fn new(kind: Kind, title: &str, options: Vec<Opt>) -> Select {
        let mut s = Select {
            kind,
            title: title.into(),
            suffix: String::new(),
            placeholder: "Search".into(),
            options,
            filter: Editor::default(),
            selected: 0,
            scroll: 0,
            flat: false,
            size: Size::Medium,
            hints: vec![],
            confirm: None,
            all: false,
            pinned: 0,
        };
        s.focus_current();
        s
    }

    pub fn focus_current(&mut self) {
        let vis = self.visible();
        if let Some(i) = vis.iter().position(|o| o.current) {
            self.selected = i;
        }
    }

    /// Visible options in display order (grouped by first appearance of each category).
    pub fn visible(&self) -> Vec<&Opt> {
        let q = self.filter.text.trim();
        if q.is_empty() {
            let mut cats: Vec<&str> = Vec::new();
            for o in &self.options {
                if !cats.contains(&o.category.as_str()) {
                    cats.push(&o.category);
                }
            }
            return cats.iter().flat_map(|c| self.options.iter().filter(move |o| o.category == *c)).collect();
        }
        let mut scored: Vec<(i64, usize, &Opt)> = self
            .options
            .iter()
            .enumerate()
            .skip(self.pinned)
            .filter_map(|(i, o)| {
                let a = fuzzy(&o.title, q).map(|s| s * 2);
                let b = fuzzy(&o.category, q);
                let c = fuzzy(&o.search, q);
                let best = [a, b, c].into_iter().flatten().max()?;
                Some((best, i, o))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let list: Vec<&Opt> = scored.into_iter().map(|(_, _, o)| o).collect();
        if self.flat {
            return list;
        }
        let mut cats: Vec<&str> = Vec::new();
        for o in &list {
            if !cats.contains(&o.category.as_str()) {
                cats.push(&o.category);
            }
        }
        cats.iter().flat_map(|c| list.iter().copied().filter(move |o| o.category == *c)).collect()
    }

    fn grouped(&self) -> bool {
        !(self.flat && !self.filter.text.trim().is_empty())
    }

    pub fn current(&self) -> Option<&Opt> {
        self.visible().get(self.selected).copied()
    }

    pub fn moved(&mut self, delta: isize) {
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = if delta.abs() == 1 { next.rem_euclid(n as isize) as usize } else { next.clamp(0, n as isize - 1) as usize };
        self.confirm = None;
    }

    pub fn set_filter(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.confirm = None;
    }
}

pub struct Prompt {
    pub title: String,
    pub editor: Editor,
    pub target: String,
}

pub enum Dialog {
    Select(Select),
    Help,
    Status,
    Rename(Prompt),
}

/// Geometry shared by drawing and mouse hit testing.
pub struct Placed {
    pub panel: Rect,
    pub list: Rect,
    /// (screen row, visible option index)
    pub rows: Vec<(u16, usize)>,
}

fn panel_rect(area: Rect, size: Size, height: u16) -> Rect {
    let w = width(size).min(area.width.saturating_sub(2));
    let x = area.x + (area.width - w) / 2;
    let top = ((area.height as f32) / 4.0).round() as u16;
    let h = height.min(area.height.saturating_sub(top));
    Rect { x, y: area.y + top, width: w, height: h }
}

fn header(t: &Tokens, title: &str, suffix: &str, close: &str, w: usize) -> Line<'static> {
    let bg = t.bg;
    let mut spans = vec![Span::styled(title.to_string(), draw::bold(draw::st(t.text, bg)))];
    if !suffix.is_empty() {
        spans.push(span(suffix.to_string(), t.muted, bg));
    }
    let used: usize = spans.iter().map(|s| draw::width(&s.content)).sum();
    spans.push(span(" ".repeat(w.saturating_sub(used + draw::width(close))), t.text, bg));
    spans.push(span(close.to_string(), t.muted, bg));
    line(spans, bg)
}

/// Draw a select dialog; returns its geometry and the filter cursor.
pub fn draw_select(buf: &mut Buffer, area: Rect, theme: &Theme, s: &mut Select) -> (Placed, (u16, u16)) {
    let t = &theme.dialog;
    let vis: Vec<Opt> = s.visible().into_iter().cloned().collect();
    let grouped = s.grouped();
    // rows: (category header?, option index)
    let mut rows: Vec<Result<(String, bool), usize>> = Vec::new();
    let mut last_cat: Option<&str> = None;
    for (i, o) in vis.iter().enumerate() {
        if grouped && !o.category.is_empty() && last_cat != Some(o.category.as_str()) {
            if last_cat.is_some() {
                rows.push(Ok((String::new(), false)));
            }
            rows.push(Ok((o.category.clone(), true)));
        }
        last_cat = Some(o.category.as_str());
        rows.push(Err(i));
    }
    let limit = (area.height as usize / 2).saturating_sub(6).max(1);
    let list_h = rows.len().clamp(1, limit);
    let footer = !s.hints.is_empty();
    let height = 1 + 1 + 1 + 1 + 1 + list_h + 1 + if footer { 1 } else { 0 } + 1;
    let panel = panel_rect(area, s.size, height as u16);
    draw::dim(buf, area, 150);
    draw::fill(buf, panel, t.bg);
    let inner_x = panel.x + INSET;
    let inner_w = panel.width.saturating_sub(INSET * 2) as usize;
    // header (title row inset by one more column, like DialogSelect)
    let hx = inner_x + (LIST_INSET - INSET);
    let hw = inner_w.saturating_sub(((LIST_INSET - INSET) * 2) as usize);
    draw::draw_line(buf, hx, panel.y + 1, hw as u16, &header(t, &s.title, &s.suffix, "esc", hw));
    let fy = panel.y + 3;
    let filter = if s.filter.text.is_empty() { line(vec![span(s.placeholder.clone(), t.muted, t.bg)], t.bg) } else { line(vec![span(s.filter.text.clone(), t.text_formfield.focused, t.bg)], t.bg) };
    draw::draw_line(buf, hx, fy, hw as u16, &filter);
    let cursor_col = draw::width(&s.filter.text[..s.filter.cursor]) as u16;
    let cursor = (hx + cursor_col.min(hw as u16), fy);

    // scrolling keeps the selection visible
    let sel_row = rows.iter().position(|r| matches!(r, Err(i) if *i == s.selected)).unwrap_or(0);
    if sel_row < s.scroll {
        s.scroll = sel_row.saturating_sub(if sel_row > 0 && matches!(rows.get(sel_row - 1), Some(Ok((_, true)))) { 1 } else { 0 });
    }
    if sel_row >= s.scroll + list_h {
        s.scroll = sel_row + 1 - list_h;
    }
    s.scroll = s.scroll.min(rows.len().saturating_sub(list_h));
    let ly = fy + 2;
    let list = Rect { x: panel.x + 1, y: ly, width: panel.width.saturating_sub(2), height: list_h as u16 };
    let mut placed = Vec::new();
    if vis.is_empty() {
        let msg = if s.filter.text.is_empty() { "No items available" } else { "No results found" };
        draw::draw_line(buf, panel.x + LIST_INSET, ly, inner_w as u16, &line(vec![span(msg, t.muted, t.bg)], t.bg));
    }
    let row_w = list.width as usize;
    for (k, r) in rows.iter().enumerate().skip(s.scroll).take(list_h) {
        let y = ly + (k - s.scroll) as u16;
        match r {
            Ok((cat, true)) => {
                let l = line(vec![span("   ", t.text, t.bg), Span::styled(cat.clone(), draw::bold(draw::st(theme.hue("accent", 200), t.bg)))], t.bg);
                draw::draw_line(buf, list.x, y, list.width, &l);
            }
            Ok(_) => {}
            Err(i) => {
                let o = &vis[*i];
                let active = *i == s.selected;
                let danger = active && s.confirm.as_deref() == Some(o.value.as_str());
                let bg = if danger {
                    t.bg_destructive.focused
                } else if active {
                    t.bg_primary.focused
                } else {
                    t.bg
                };
                let fg = if danger {
                    t.text_destructive.focused
                } else if active {
                    t.text_primary.focused
                } else if o.current {
                    t.text_formfield.selected
                } else {
                    t.text
                };
                let secondary = if active { fg } else { t.muted };
                let mut spans = Vec::new();
                if o.current {
                    spans.push(span(" ", fg, bg));
                    spans.push(span("● ", fg, bg));
                } else {
                    spans.push(span("   ", fg, bg));
                }
                let title = if danger { "Press ctrl+d again to confirm".to_string() } else { draw::truncate(&o.title, 61) };
                let mut tstyle = draw::st(fg, bg);
                if active {
                    tstyle = draw::bold(tstyle);
                }
                let footer_w = draw::width(&o.footer);
                let avail = row_w.saturating_sub(3 + 3 + if footer_w > 0 { footer_w + 1 } else { 0 });
                let mut text = vec![Span::styled(title, tstyle)];
                if !o.description.is_empty() && !danger {
                    // the highlighted row is bold throughout, description included
                    text.push(Span::styled(format!(" {}", o.description), if active { tstyle } else { draw::st(secondary, bg) }));
                }
                let clipped: Vec<Span<'static>> = draw::wrap(&text, avail.max(1), draw::Wrap::None).into_iter().next().unwrap_or_default();
                let mut used = 0;
                for sp in clipped {
                    let w = draw::width(&sp.content);
                    if used + w > avail {
                        let keep = draw::take_width(&sp.content, avail - used);
                        used += draw::width(&keep);
                        spans.push(Span::styled(keep, sp.style));
                        break;
                    }
                    used += w;
                    spans.push(sp);
                }
                spans.push(span(" ".repeat(avail.saturating_sub(used)), fg, bg));
                if footer_w > 0 {
                    spans.push(span(" ", fg, bg));
                    spans.push(span(o.footer.clone(), secondary, bg));
                }
                spans.push(span("   ", fg, bg));
                draw::draw_line(buf, list.x, y, list.width, &line(spans, bg));
                placed.push((y, *i));
            }
        }
    }
    if footer {
        let y = ly + list_h as u16 + 1;
        let item = |h: &Hint| vec![span(h.title.clone(), t.text, t.bg), span(format!(" {}", h.key), t.muted, t.bg)];
        let mut left: Vec<Span<'static>> = Vec::new();
        for h in s.hints.iter().filter(|h| !h.right) {
            if !left.is_empty() {
                left.push(span("  ", t.text, t.bg));
            }
            left.extend(item(h));
        }
        let mut right: Vec<Span<'static>> = Vec::new();
        for h in s.hints.iter().filter(|h| h.right) {
            if !right.is_empty() {
                right.push(span("  ", t.text, t.bg));
            }
            right.extend(item(h));
        }
        let fw = inner_w.saturating_sub(2);
        let lw: usize = left.iter().map(|x| draw::width(&x.content)).sum();
        let rw: usize = right.iter().map(|x| draw::width(&x.content)).sum();
        left.push(span(" ".repeat(fw.saturating_sub(lw + rw)), t.text, t.bg));
        left.extend(right);
        draw::draw_line(buf, inner_x + 1, y, fw as u16, &line(left, t.bg));
    }
    (Placed { panel, list, rows: placed }, cursor)
}

/// A text dialog: title, body lines and an `ok` button (Help).
pub fn draw_text(buf: &mut Buffer, area: Rect, theme: &Theme, title: &str, close: &str, body: Vec<Line<'static>>, ok: bool) -> Rect {
    let t = &theme.dialog;
    let h = 1 + 1 + 1 + body.len() + if ok { 2 } else { 0 } + 1;
    let panel = panel_rect(area, Size::Medium, h as u16);
    draw::dim(buf, area, 150);
    draw::fill(buf, panel, t.bg);
    let x = panel.x + INSET;
    let w = panel.width.saturating_sub(INSET * 2);
    draw::draw_line(buf, x, panel.y + 1, w, &header(t, title, "", close, w as usize));
    for (i, l) in body.iter().enumerate() {
        draw::draw_line(buf, x, panel.y + 3 + i as u16, w, l);
    }
    if ok {
        let y = panel.y + 3 + body.len() as u16 + 1;
        let label = "   ok   ";
        let bx = x + w.saturating_sub(label.len() as u16);
        draw::draw_line(buf, bx, y, label.len() as u16, &line(vec![span(label, t.text_primary.focused, t.bg_primary.focused)], t.bg_primary.focused));
    }
    panel
}

/// `DialogPrompt`: title, a single-line field and an enter hint.
pub fn draw_prompt(buf: &mut Buffer, area: Rect, theme: &Theme, p: &Prompt) -> (u16, u16) {
    let t = &theme.dialog;
    let panel = panel_rect(area, Size::Medium, 7);
    draw::dim(buf, area, 150);
    draw::fill(buf, panel, t.bg);
    let x = panel.x + INSET;
    let w = panel.width.saturating_sub(INSET * 2);
    draw::draw_line(buf, x, panel.y + 1, w, &header(t, &p.title, "", "esc", w as usize));
    draw::draw_line(buf, x, panel.y + 3, w, &line(vec![span(p.editor.text.clone(), t.text, t.bg)], t.bg));
    let hint = line(vec![span("enter ", t.text, t.bg), span("submit", t.muted, t.bg)], t.bg);
    draw::draw_line(buf, x, panel.y + 5, w, &hint);
    (x + draw::width(&p.editor.text[..p.editor.cursor]).min(w as usize) as u16, panel.y + 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_and_filtering() {
        let opts = vec![
            Opt { title: "Switch model".into(), category: "Agent".into(), value: "model.list".into(), ..Default::default() },
            Opt { title: "New session".into(), category: "Session".into(), value: "session.new".into(), ..Default::default() },
            Opt { title: "Switch agent".into(), category: "Agent".into(), value: "agent.list".into(), ..Default::default() },
        ];
        let mut s = Select::new(Kind::Palette, "Commands", opts);
        let order: Vec<&str> = s.visible().iter().map(|o| o.value.as_str()).collect();
        assert_eq!(order, ["model.list", "agent.list", "session.new"]);
        s.filter.insert("new");
        assert_eq!(s.visible()[0].value, "session.new");
    }
}
