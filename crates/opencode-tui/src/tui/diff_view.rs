//! Full-screen diff viewer modeled on upstream `feature-plugins/system/diff-viewer`:
//! a file tree, unified or split patches, wrapping, single-patch mode, source
//! switching, reviewed marks and hunk/file navigation.

use super::draw::{self, line, span};
use super::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Span;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Working,
    Branch,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Working => "working",
            Source::Branch => "branch",
        }
    }
    pub fn next(self) -> Source {
        match self {
            Source::Working => Source::Branch,
            Source::Branch => Source::Working,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Context,
    Add,
    Del,
    Hunk,
}

struct Row {
    kind: Kind,
    old: Option<usize>,
    new: Option<usize>,
    text: String,
}

struct File {
    path: String,
    status: String,
    additions: usize,
    deletions: usize,
    rows: Vec<Row>,
}

pub struct DiffView {
    files: Vec<File>,
    pub source: Source,
    pub selected: usize,
    pub scroll: usize,
    pub tree: bool,
    pub single: bool,
    pub split: bool,
    pub fullscreen: bool,
    pub wrap: bool,
    pub help: bool,
    pub loading: bool,
    reviewed: HashSet<String>,
}

impl DiffView {
    pub fn new(source: Source, tree: bool, single: bool, split: bool, fullscreen: bool, wrap: bool) -> DiffView {
        DiffView {
            files: Vec::new(),
            source,
            selected: 0,
            scroll: 0,
            tree,
            single,
            split,
            fullscreen,
            wrap,
            help: false,
            loading: true,
            reviewed: HashSet::new(),
        }
    }

    pub fn set_files(&mut self, files: Value) {
        self.files = files
            .as_array()
            .map(|items| items.iter().filter_map(file).collect())
            .unwrap_or_default();
        self.loading = false;
        self.selected = self.selected.min(self.files.len().saturating_sub(1));
        self.scroll = 0;
    }

    pub fn next_file(&mut self, step: isize) {
        if self.files.is_empty() {
            return;
        }
        let n = self.files.len() as isize;
        self.selected = (self.selected as isize + step).rem_euclid(n) as usize;
        self.scroll = 0;
    }

    pub fn next_hunk(&mut self, forward: bool) {
        let Some(file) = self.files.get(self.selected) else { return };
        let current = self.scroll;
        let next = if forward {
            file.rows.iter().enumerate().skip(current + 1).find(|(_, row)| row.kind == Kind::Hunk).map(|(i, _)| i)
        } else {
            file.rows.iter().enumerate().take(current).rev().find(|(_, row)| row.kind == Kind::Hunk).map(|(i, _)| i)
        };
        if let Some(next) = next {
            self.scroll = next;
        }
    }

    pub fn toggle_reviewed(&mut self) {
        let Some(file) = self.files.get(self.selected) else { return };
        if !self.reviewed.remove(&file.path) {
            self.reviewed.insert(file.path.clone());
        }
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let margin = if self.fullscreen { 0 } else { 1 };
        let area = Rect {
            x: area.x + margin,
            y: area.y + margin,
            width: area.width.saturating_sub(margin * 2),
            height: area.height.saturating_sub(margin * 2),
        };
        if area.width < 20 || area.height < 4 {
            return;
        }
        let bg = t.bg;
        draw::fill(buf, area, bg);
        let title = format!(
            "Diff · {} · {} file{} · {}",
            self.source.name(),
            self.files.len(),
            if self.files.len() == 1 { "" } else { "s" },
            if self.loading { "loading…" } else { "esc close" }
        );
        draw::draw_line(buf, area.x, area.y, area.width, &line(vec![Span::styled(title, draw::bold(draw::st(t.text, bg)))], bg));
        let footer = "j/k scroll  n/p file  ]/[ hunk  b tree  s single  v split  d source  f full  m reviewed  ? help";
        draw::draw_line(buf, area.x, area.y + area.height.saturating_sub(1), area.width, &line(vec![span(draw::take_width(footer, area.width as usize), t.muted, bg)], bg));
        let body = Rect { x: area.x, y: area.y + 1, width: area.width, height: area.height.saturating_sub(2) };
        if self.help {
            self.draw_help(buf, body, t);
            return;
        }
        let tree_w = if self.tree && body.width >= 48 { 34.min(body.width / 3) } else { 0 };
        if tree_w > 0 {
            self.draw_tree(buf, Rect { x: body.x, y: body.y, width: tree_w, height: body.height }, t);
        }
        let patch = Rect { x: body.x + tree_w + if tree_w > 0 { 1 } else { 0 }, y: body.y, width: body.width.saturating_sub(tree_w + if tree_w > 0 { 1 } else { 0 }), height: body.height };
        if self.files.is_empty() {
            let msg = if self.loading { "Loading diff…" } else { "No changes" };
            draw::draw_line(buf, patch.x, patch.y, patch.width, &line(vec![span(msg, t.muted, bg)], bg));
            return;
        }
        if self.split && patch.width >= 80 {
            self.draw_split(buf, patch, t);
        } else {
            self.draw_unified(buf, patch, t);
        }
    }

    fn draw_tree(&self, buf: &mut Buffer, area: Rect, t: &Theme) {
        draw::fill(buf, area, t.raised);
        for (index, file) in self.files.iter().enumerate().take(area.height as usize) {
            let y = area.y + index as u16;
            let selected = index == self.selected;
            let bg = if selected { t.raised_high } else { t.raised };
            let mark = if self.reviewed.contains(&file.path) { "✓" } else { " " };
            let status = match file.status.as_str() {
                "added" => "+",
                "deleted" => "-",
                _ => "~",
            };
            let color = match file.status.as_str() {
                "added" => t.diff.added,
                "deleted" => t.diff.removed,
                _ => t.text,
            };
            let path = draw::take_width(&file.path, area.width.saturating_sub(8) as usize);
            let spans = vec![
                span(" ", t.text, bg),
                span(mark, t.success.base, bg),
                span(status, color, bg),
                span(" ", t.text, bg),
                Span::styled(path, draw::st(if selected { t.text } else { t.muted }, bg)),
            ];
            draw::draw_line(buf, area.x, y, area.width, &line(spans, bg));
        }
    }

    fn draw_unified(&self, buf: &mut Buffer, patch: Rect, t: &Theme) {
        let files: Vec<&File> = if self.single {
            self.files.get(self.selected).into_iter().collect()
        } else {
            self.files.iter().collect()
        };
        let mut y = patch.y;
        let bottom = patch.y + patch.height;
        for (index, file) in files.iter().enumerate() {
            if y >= bottom {
                break;
            }
            if !self.single {
                if index > 0 {
                    y += 1;
                    if y >= bottom {
                        break;
                    }
                }
                let header = format!("{}  {} +{} -{}", file.path, file.status, file.additions, file.deletions);
                draw::draw_line(buf, patch.x, y, patch.width, &line(vec![Span::styled(draw::take_width(&header, patch.width as usize), draw::bold(draw::st(t.text, t.raised)))], t.raised));
                y += 1;
            }
            for row in &file.rows {
                if y >= bottom {
                    break;
                }
                self.draw_row(buf, patch, y, row, t);
                y += 1;
            }
        }
        if self.scroll > 0 {
            let label = format!("↕ {}", self.scroll);
            draw::draw_line(buf, patch.x + patch.width.saturating_sub(label.len() as u16), patch.y, label.len() as u16, &line(vec![span(label, t.muted, t.bg)], t.bg));
        }
    }

    fn draw_split(&self, buf: &mut Buffer, patch: Rect, t: &Theme) {
        let Some(file) = self.files.get(self.selected) else { return };
        let half = patch.width / 2;
        let left = Rect { x: patch.x, y: patch.y, width: half.saturating_sub(1), height: patch.height };
        let right = Rect { x: patch.x + half, y: patch.y, width: patch.width.saturating_sub(half), height: patch.height };
        draw::draw_line(buf, left.x, left.y, left.width, &line(vec![Span::styled(draw::take_width(&file.path, left.width as usize), draw::bold(draw::st(t.text, t.raised)))], t.raised));
        draw::draw_line(buf, right.x, right.y, right.width, &line(vec![Span::styled("new", draw::bold(draw::st(t.text, t.raised)))], t.raised));
        for (index, row) in file.rows.iter().skip(self.scroll).take(patch.height.saturating_sub(1) as usize).enumerate() {
            let y = patch.y + 1 + index as u16;
            let (old, new) = (row.old, row.new);
            let old_text = if row.kind == Kind::Add { String::new() } else { row.text.clone() };
            let new_text = if row.kind == Kind::Del { String::new() } else { row.text.clone() };
            let (old_bg, new_bg) = match row.kind {
                Kind::Add => (t.bg, t.diff.bg_added),
                Kind::Del => (t.diff.bg_removed, t.bg),
                Kind::Hunk => (t.diff.bg_context, t.diff.bg_context),
                _ => (t.bg, t.bg),
            };
            let old_color = if row.kind == Kind::Del { t.diff.removed } else { t.muted };
            let new_color = if row.kind == Kind::Add { t.diff.added } else { t.muted };
            draw::draw_line(buf, left.x, y, left.width, &line(vec![span(format!("{:>4} ", old.map(|n| n.to_string()).unwrap_or_default()), old_color, old_bg), Span::styled(draw::take_width(&old_text, left.width.saturating_sub(5) as usize), draw::st(old_color, old_bg))], old_bg));
            draw::draw_line(buf, right.x, y, right.width, &line(vec![span(format!("{:>4} ", new.map(|n| n.to_string()).unwrap_or_default()), new_color, new_bg), Span::styled(draw::take_width(&new_text, right.width.saturating_sub(5) as usize), draw::st(new_color, new_bg))], new_bg));
        }
    }

    fn draw_row(&self, buf: &mut Buffer, patch: Rect, y: u16, row: &Row, t: &Theme) {
        let (bg, fg) = match row.kind {
            Kind::Add => (t.diff.bg_added, t.diff.added),
            Kind::Del => (t.diff.bg_removed, t.diff.removed),
            Kind::Hunk => (t.diff.bg_context, t.diff.hunk_header),
            Kind::Context => (t.bg, t.text),
        };
        let prefix = match row.kind {
            Kind::Add => "+",
            Kind::Del => "-",
            Kind::Hunk => "",
            _ => " ",
        };
        let old = row.old.map(|n| n.to_string()).unwrap_or_default();
        let new = row.new.map(|n| n.to_string()).unwrap_or_default();
        let number = format!("{:>4} {:>4} ", old, new);
        let text = if self.wrap {
            draw::wrap(&[Span::styled(row.text.clone(), draw::st(fg, bg))], patch.width.saturating_sub(number.len() as u16) as usize, draw::Wrap::Word)
                .into_iter()
                .next()
                .map(|parts| parts.into_iter().map(|part| part.content).collect::<String>())
                .unwrap_or_default()
        } else {
            draw::take_width(&row.text, patch.width.saturating_sub(number.len() as u16 + 1) as usize)
        };
        let mut spans = vec![span(number, t.muted, bg), span(prefix, fg, bg), Span::styled(text, draw::st(fg, bg))];
        if row.kind == Kind::Hunk {
            spans = vec![Span::styled(draw::take_width(&row.text, patch.width as usize), draw::bold(draw::st(fg, bg)))];
        }
        draw::draw_line(buf, patch.x, y, patch.width, &line(spans, bg));
    }

    fn draw_help(&self, buf: &mut Buffer, area: Rect, t: &Theme) {
        draw::fill(buf, area, t.bg);
        let lines = [
            "Diff viewer",
            "",
            "j / down        scroll down",
            "k / up          scroll up",
            "ctrl+d / ctrl+u half page",
            "g / home        first hunk",
            "G / end         last hunk",
            "] / [           next / previous hunk",
            "n / p           next / previous file",
            "b               toggle file tree",
            "s               single patch",
            "v               split / unified",
            "d               switch source",
            "f               full screen",
            "m               mark file reviewed",
            "?               close help",
            "q / esc         close viewer",
        ];
        for (index, text) in lines.iter().enumerate() {
            let y = area.y + index as u16;
            if y >= area.y + area.height {
                break;
            }
            let style = if index == 0 { draw::bold(draw::st(t.text, t.bg)) } else { draw::st(t.muted, t.bg) };
            draw::draw_line(buf, area.x + 2, y, area.width.saturating_sub(4), &line(vec![Span::styled((*text).to_string(), style)], t.bg));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unified_patch_rows() {
        let rows = parse_patch("@@ -1,2 +1,2 @@\n-old\n+new\n same\n");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, Kind::Hunk);
        assert_eq!(rows[1].kind, Kind::Del);
        assert_eq!(rows[2].kind, Kind::Add);
        assert_eq!(rows[3].kind, Kind::Context);
        assert_eq!(rows[3].old, Some(2));
        assert_eq!(rows[3].new, Some(2));
    }
}

fn file(value: &Value) -> Option<File> {
    let path = value.get("file")?.as_str()?.to_string();
    let patch = value.get("patch").and_then(Value::as_str).unwrap_or_default();
    Some(File {
        path,
        status: value.get("status").and_then(Value::as_str).unwrap_or("modified").to_string(),
        additions: value.get("additions").and_then(Value::as_u64).unwrap_or(0) as usize,
        deletions: value.get("deletions").and_then(Value::as_u64).unwrap_or(0) as usize,
        rows: parse_patch(patch),
    })
}

fn parse_patch(patch: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut old = 0usize;
    let mut new = 0usize;
    for text in patch.lines() {
        if text.starts_with("diff --git") || text.starts_with("index ") || text.starts_with("--- ") || text.starts_with("+++ ") || text.starts_with("new file mode") || text.starts_with("deleted file mode") || text.starts_with("similarity index") {
            continue;
        }
        if let Some(rest) = text.strip_prefix("@@") {
            let mut parts = rest.split_whitespace();
            if let Some(range) = parts.next().and_then(|v| v.strip_prefix('-')) {
                old = range.split(',').next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            if let Some(range) = parts.next().and_then(|v| v.strip_prefix('+')) {
                new = range.split(',').next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            rows.push(Row { kind: Kind::Hunk, old: None, new: None, text: text.to_string() });
            continue;
        }
        if text.starts_with('\\') {
            continue;
        }
        if let Some(rest) = text.strip_prefix('+') {
            rows.push(Row { kind: Kind::Add, old: None, new: Some(new), text: rest.to_string() });
            new += 1;
        } else if let Some(rest) = text.strip_prefix('-') {
            rows.push(Row { kind: Kind::Del, old: Some(old), new: None, text: rest.to_string() });
            old += 1;
        } else if let Some(rest) = text.strip_prefix(' ') {
            rows.push(Row { kind: Kind::Context, old: Some(old), new: Some(new), text: rest.to_string() });
            old += 1;
            new += 1;
        }
    }
    rows
}
