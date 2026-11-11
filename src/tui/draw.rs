//! Drawing primitives: styled segments, opentui-style wrapping and buffer blitting.

use super::theme::{color_of, Rgba};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Style from a foreground composited over an opaque background.
pub fn st(fg: Rgba, bg: Rgba) -> Style {
    Style::default().fg(fg.over(bg).color()).bg(bg.color())
}

pub fn bold(s: Style) -> Style {
    s.add_modifier(Modifier::BOLD)
}

pub fn span(text: impl Into<String>, fg: Rgba, bg: Rgba) -> Span<'static> {
    Span::styled(text.into(), st(fg, bg))
}

/// A line whose unused cells are filled with `bg`.
pub fn line(spans: Vec<Span<'static>>, bg: Rgba) -> Line<'static> {
    Line::from(spans).style(Style::default().bg(bg.color()))
}

pub fn blank(bg: Rgba) -> Line<'static> {
    line(vec![], bg)
}

pub fn line_width(l: &Line) -> usize {
    l.spans.iter().map(|s| width(&s.content)).sum()
}

/// Prefix every line with `spans`.
pub fn indent(lines: Vec<Line<'static>>, prefix: &[Span<'static>]) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            let mut s = prefix.to_vec();
            s.append(&mut l.spans);
            l.spans = s;
            l
        })
        .collect()
}

/// `Locale.truncateWidth`.
pub fn truncate(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    if width(s) <= w {
        return s.to_string();
    }
    if w == 1 {
        return "…".into();
    }
    format!("{}…", take_width(s, w - 1))
}

pub fn take_width(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out
}

/// `Locale.truncateMiddle` (character based, like upstream).
pub fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if max == 0 {
        return String::new();
    }
    if chars.len() <= max {
        return s.to_string();
    }
    let start = (max - 1).div_ceil(2);
    let end = (max - 1) / 2;
    if end == 0 {
        return chars[..start].iter().collect::<String>() + "…";
    }
    chars[..start].iter().collect::<String>() + "…" + &chars[chars.len() - end..].iter().collect::<String>()
}

fn take_end(s: &str, w: usize) -> String {
    let mut out: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.into_iter().rev().collect()
}

fn truncate_basename(v: &str, w: usize) -> String {
    if width(v) <= w {
        return v.to_string();
    }
    if w <= 1 {
        return take_width("…", w);
    }
    let dot = v.rfind('.').filter(|d| *d > 0);
    let ext = dot.map(|d| &v[d..]).unwrap_or("");
    let ew = width(ext);
    if ew >= w {
        return format!("…{}", take_end(ext, w - 1));
    }
    let stem = dot.map(|d| &v[..d]).unwrap_or(v);
    format!("{}…{ext}", take_width(stem, w - ew - 1))
}

/// `truncateFilePath`: keep the root and basename, drop middle segments: `…/fun stu…/opencode`.
pub fn truncate_path(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(value) <= max {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/');
    let windows = drive || (!value.contains('/') && value.contains('\\'));
    let sep = if drive { bytes[2] as char } else if windows { '\\' } else { '/' };
    let (root, source) = if drive {
        (value[..3].to_string(), &value[3..])
    } else if value.starts_with('/') {
        ("/".to_string(), &value[1..])
    } else {
        (String::new(), value)
    };
    let segments: Vec<&str> = source.split(|c| c == sep || (windows && (c == '/' || c == '\\'))).filter(|s| !s.is_empty()).collect();
    let basename = segments.last().copied().unwrap_or(value);
    if segments.len() < 2 {
        let rw = width(&root);
        if rw >= max {
            return take_width(&root, max);
        }
        return root.clone() + &truncate_basename(basename, max - rw);
    }
    let prefix = format!("{root}…{sep}");
    let bw = max.saturating_sub(width(&prefix));
    if bw == 0 {
        return take_width(&prefix, max);
    }
    let compact = truncate_basename(basename, bw);
    if compact != basename {
        return prefix + &compact;
    }
    let mut selected = vec![basename.to_string()];
    let mut used = width(&prefix) + width(basename);
    for seg in segments[..segments.len() - 1].iter().rev() {
        let next = width(seg) + 1;
        if used + next > max {
            let avail = max as isize - used as isize - 1;
            if avail > 1 {
                selected.insert(0, format!("{}…", take_width(seg, avail as usize - 1)));
            }
            break;
        }
        selected.insert(0, seg.to_string());
        used += next;
    }
    prefix + &selected.join(&sep.to_string())
}

/// `abbreviateHome`: `~/rest` with forward slashes when under home.
pub fn abbreviate_home(input: &str, home: &str) -> String {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let (i, h) = (norm(input), norm(home));
    if h.is_empty() {
        return input.to_string();
    }
    if i == h {
        return "~".into();
    }
    if i.starts_with(&(h.clone() + "/")) {
        let rest = input.replace('\\', "/");
        return format!("~/{}", &rest[h.len() + 1..]);
    }
    input.to_string()
}

/// `formatPath`: relative to `base` when inside it, otherwise home-abbreviated.
pub fn format_path(input: &str, base: &str, home: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let p = std::path::Path::new(input);
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::path::Path::new(base).join(p) };
    let a = abs.to_string_lossy().replace('\\', "/");
    let b = base.replace('\\', "/").trim_end_matches('/').to_string();
    if a.eq_ignore_ascii_case(&b) {
        return ".".into();
    }
    if a.len() > b.len() && a[..b.len()].eq_ignore_ascii_case(&b) && a.as_bytes()[b.len()] == b'/' {
        let rel = &abs.to_string_lossy()[b.len() + 1..];
        return rel.to_string();
    }
    abbreviate_home(&abs.to_string_lossy(), home)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    Word,
    Char,
    None,
}

fn breaks_after(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '/' | '\\' | '.' | ',' | ';' | ':' | '!' | '?')
}

/// Wrap styled spans to `width` columns. Hard newlines start new lines.
pub fn wrap(spans: &[Span<'static>], width: usize, mode: Wrap) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    // flatten to (char, style)
    let mut lines: Vec<Vec<(char, Style)>> = vec![vec![]];
    for s in spans {
        for c in s.content.chars() {
            if c == '\n' {
                lines.push(vec![]);
            } else if c == '\t' {
                // opentui text buffers draw a tab two columns wide
                lines.last_mut().unwrap().extend([(' ', s.style), (' ', s.style)]);
            } else if c != '\r' {
                lines.last_mut().unwrap().push((c, s.style));
            }
        }
    }
    let mut out = Vec::new();
    for chars in lines {
        if mode == Wrap::None {
            out.push(chars);
            continue;
        }
        let mut start = 0;
        if chars.is_empty() {
            out.push(vec![]);
            continue;
        }
        while start < chars.len() {
            let mut w = 0;
            let mut end = start;
            let mut last_break: Option<usize> = None;
            while end < chars.len() {
                let cw = char_width(chars[end].0);
                if w + cw > width {
                    break;
                }
                w += cw;
                if mode == Wrap::Word && breaks_after(chars[end].0) {
                    last_break = Some(end + 1);
                }
                end += 1;
            }
            if end >= chars.len() {
                out.push(chars[start..].to_vec());
                break;
            }
            // a space right at the boundary can be dropped instead of moving the word
            if mode == Wrap::Word && chars[end].0 == ' ' {
                out.push(chars[start..end].to_vec());
                start = end + 1;
                continue;
            }
            let cut = match last_break {
                Some(b) if mode == Wrap::Word && b > start => b,
                _ => end.max(start + 1),
            };
            let mut seg = chars[start..cut].to_vec();
            while seg.last().is_some_and(|(c, _)| *c == ' ') && mode == Wrap::Word {
                seg.pop();
            }
            out.push(seg);
            start = cut;
        }
    }
    out.into_iter().map(group).collect()
}

fn group(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for (c, s) in chars {
        match out.last_mut() {
            Some(last) if last.style == s => last.content.to_mut().push(c),
            _ => out.push(Span::styled(c.to_string(), s)),
        }
    }
    out
}

/// Wrap and turn into filled lines with a left padding and continuation padding.
pub fn paragraph(spans: &[Span<'static>], width: usize, mode: Wrap, bg: Rgba) -> Vec<Line<'static>> {
    wrap(spans, width, mode).into_iter().map(|s| line(s, bg)).collect()
}

/// Draw one line at (x, y) clipped to `w` columns; the line style's bg fills the row.
pub fn draw_line(buf: &mut Buffer, x: u16, y: u16, w: u16, l: &Line) {
    let area = buf.area;
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let right = (x + w).min(area.x + area.width);
    if l.style.bg.is_some() {
        for cx in x..right {
            let cell = &mut buf[(cx, y)];
            cell.set_symbol(" ");
            cell.modifier = ratatui::style::Modifier::empty();
            cell.set_style(Style::default().bg(l.style.bg.unwrap()));
        }
    }
    let mut cx = x;
    for s in &l.spans {
        for ch in s.content.chars() {
            let cw = char_width(ch) as u16;
            if cw == 0 {
                continue;
            }
            if cx + cw > right {
                return;
            }
            let cell = &mut buf[(cx, y)];
            cell.set_char(ch);
            cell.modifier = ratatui::style::Modifier::empty();
            cell.set_style(s.style);
            if cw == 2 && cx + 1 < right {
                buf[(cx + 1, y)].set_symbol("");
            }
            cx += cw;
        }
    }
}

pub fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[Line], skip: usize) {
    for (i, l) in lines.iter().skip(skip).take(area.height as usize).enumerate() {
        draw_line(buf, area.x, area.y + i as u16, area.width, l);
    }
}

pub fn fill(buf: &mut Buffer, area: Rect, bg: Rgba) {
    let area = area.intersection(buf.area);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            cell.set_symbol(" ");
            cell.modifier = ratatui::style::Modifier::empty();
            cell.set_style(Style::default().bg(bg.color()));
        }
    }
}

/// Translucent black overlay (`RGBA.fromInts(0, 0, 0, alpha)`), as behind dialogs.
pub fn dim(buf: &mut Buffer, area: Rect, alpha: u8) {
    let a = alpha as f64 / 255.0;
    let area = area.intersection(buf.area);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            let f = |c: Option<ratatui::style::Color>| c.map(|c| super::theme::tint(color_of(c), Rgba::BLACK, a).color());
            let fg = f(Some(cell.fg));
            let bg = f(Some(cell.bg));
            if let Some(fg) = fg {
                cell.fg = fg;
            }
            if let Some(bg) = bg {
                cell.bg = bg;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: Vec<Vec<Span<'static>>>) -> Vec<String> {
        lines.into_iter().map(|l| l.iter().map(|s| s.content.to_string()).collect()).collect()
    }

    /// Matches upstream's inline tool snapshot at 72 columns (67 for the label).
    #[test]
    fn word_wrap_matches_opentui() {
        let s = vec![Span::raw("Grep \"OPENCODE.*DB|database|sqlite|drizzle|dev.*db|data.*dir|xdg|APPDATA\" in packages/opencode/src (151 matches)")];
        assert_eq!(plain(wrap(&s, 67, Wrap::Word))[0], "Grep \"OPENCODE.*DB|database|sqlite|drizzle|dev.*db|data.");
        let s = vec![Span::raw("Grep \"export const OPENCODE_DB|OPENCODE_DB|OPENCODE_DEV|Global\\.Path\\.data|data =\" in packages/opencode/src (115 matches)")];
        let l = plain(wrap(&s, 67, Wrap::Word));
        assert_eq!(l[0], "Grep \"export const OPENCODE_DB|OPENCODE_DB|OPENCODE_DEV|Global\\.");
        assert_eq!(l[1], "Path\\.data|data =\" in packages/opencode/src (115 matches)");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        // captured from the TypeScript prompt footer
        assert_eq!(truncate_path("~/Desktop/fun stuff/opencode/opencode-rs:master", 38), "…/fun stu…/opencode/opencode-rs:master");
        assert_eq!(abbreviate_home(r"C:\Users\me\Desktop\x", r"C:\Users\me"), "~/Desktop/x");
    }
}
