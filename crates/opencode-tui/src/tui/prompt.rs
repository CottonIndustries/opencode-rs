//! The composer: a multi-line editor, prompt history shared with the TypeScript client
//! (`<state>/prompt-history.jsonl`), paste summaries, shell mode and `/` `@` autocomplete.

use super::draw::{self, char_width};
use serde_json::{json, Value};

#[derive(Default, Clone)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
}

impl Editor {
    fn snapshot(&mut self) {
        if self.undo.last().is_none_or(|(t, _)| *t != self.text) {
            self.undo.push((self.text.clone(), self.cursor));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }
    pub fn insert(&mut self, s: &str) {
        self.snapshot();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }
    pub fn set(&mut self, s: &str) {
        self.snapshot();
        self.text = s.to_string();
        self.cursor = self.text.len();
    }
    pub fn clear(&mut self) {
        self.set("");
    }
    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().last().map(|(j, _)| j).unwrap_or(0)
    }
    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.snapshot();
            let p = self.prev(self.cursor);
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.snapshot();
            let n = self.next(self.cursor);
            self.text.replace_range(self.cursor..n, "");
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }
    pub fn right(&mut self) {
        self.cursor = self.next(self.cursor);
    }
    pub fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }
    pub fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }
    fn word_class(c: char) -> u8 {
        if c.is_whitespace() {
            0
        } else if c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    }
    fn word_back(&self) -> usize {
        let chars: Vec<(usize, char)> = self.text[..self.cursor].char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let cls = Self::word_class(chars[i - 1].1);
        while i > 0 && Self::word_class(chars[i - 1].1) == cls {
            i -= 1;
        }
        chars.get(i).map(|(b, _)| *b).unwrap_or(0)
    }
    fn word_forward(&self) -> usize {
        let rest = &self.text[self.cursor..];
        let chars: Vec<(usize, char)> = rest.char_indices().collect();
        let mut i = 0;
        while i < chars.len() && chars[i].1.is_whitespace() {
            i += 1;
        }
        if i < chars.len() {
            let cls = Self::word_class(chars[i].1);
            while i < chars.len() && Self::word_class(chars[i].1) == cls {
                i += 1;
            }
        }
        self.cursor + chars.get(i).map(|(b, _)| *b).unwrap_or(rest.len())
    }
    pub fn word_left(&mut self) {
        self.cursor = self.word_back();
    }
    pub fn word_right(&mut self) {
        self.cursor = self.word_forward();
    }
    pub fn delete_word_back(&mut self) {
        let s = self.word_back();
        if s < self.cursor {
            self.snapshot();
            self.text.replace_range(s..self.cursor, "");
            self.cursor = s;
        }
    }
    pub fn delete_word_forward(&mut self) {
        let e = self.word_forward();
        if e > self.cursor {
            self.snapshot();
            self.text.replace_range(self.cursor..e, "");
        }
    }
    pub fn delete_to_start(&mut self) {
        let s = self.line_start();
        if s < self.cursor {
            self.snapshot();
            self.text.replace_range(s..self.cursor, "");
            self.cursor = s;
        }
    }
    pub fn delete_to_end(&mut self) {
        let e = self.line_end();
        if e > self.cursor {
            self.snapshot();
            self.text.replace_range(self.cursor..e, "");
        } else if e < self.text.len() {
            self.delete();
        }
    }
    pub fn delete_line(&mut self) {
        self.snapshot();
        let s = self.line_start();
        let e = self.line_end();
        let e = if e < self.text.len() { e + 1 } else { e };
        let s = if e == self.text.len() && s > 0 { s - 1 } else { s };
        self.text.replace_range(s..e, "");
        self.cursor = s.min(self.text.len());
    }
    pub fn undo(&mut self) {
        if let Some((t, c)) = self.undo.pop() {
            self.redo.push((std::mem::replace(&mut self.text, t), self.cursor));
            self.cursor = c.min(self.text.len());
        }
    }
    pub fn redo(&mut self) {
        if let Some((t, c)) = self.redo.pop() {
            self.undo.push((std::mem::replace(&mut self.text, t), self.cursor));
            self.cursor = c.min(self.text.len());
        }
    }

    /// Visual rows (word-wrapped to `width` like the upstream textarea's `wrapMode="word"`)
    /// and the cursor's (row, col).
    pub fn layout(&self, width: usize) -> (Vec<(usize, String)>, (usize, usize)) {
        let width = width.max(1);
        // byte ranges of the visual rows
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut start = offset;
            let mut w = 0;
            // byte just after the last break opportunity in the row, and the row width up to it
            let mut brk: Option<(usize, usize)> = None;
            for (i, c) in line.char_indices() {
                let pos = offset + i;
                let cw = char_width(c);
                if w > 0 && w + cw > width {
                    match brk.take() {
                        Some((b, bw)) if b > start => {
                            spans.push((start, b));
                            start = b;
                            w -= bw;
                        }
                        _ => {
                            spans.push((start, pos));
                            start = pos;
                            w = 0;
                        }
                    }
                }
                w += cw;
                if c.is_whitespace() || matches!(c, '-' | '/' | '\\' | '.' | ',' | ';' | ':' | '!' | '?') {
                    brk = Some((pos + c.len_utf8(), w));
                }
            }
            let end = offset + line.len();
            spans.push((start, end));
            // a cursor after a full last row sits at the start of a new one
            if self.cursor == end && w >= width {
                spans.push((end, end));
            }
            offset = end + 1;
        }
        let row = spans.iter().rposition(|&(s, e)| s <= self.cursor && self.cursor <= e).unwrap_or(0);
        let (s, _) = spans[row];
        let col = self.text[s..self.cursor.max(s)].chars().map(char_width).sum();
        let rows = spans.into_iter().map(|(s, e)| (s, self.text[s..e].to_string())).collect();
        (rows, (row, col))
    }

    /// Move one visual row up/down; false at the first/last row.
    pub fn vertical(&mut self, width: usize, down: bool) -> bool {
        let (rows, (r, c)) = self.layout(width);
        let target = if down { r + 1 } else if r == 0 { return false } else { r - 1 };
        let Some((start, text)) = rows.get(target) else { return false };
        let mut col = 0;
        let mut byte = *start;
        for ch in text.chars() {
            if col >= c {
                break;
            }
            col += char_width(ch);
            byte += ch.len_utf8();
        }
        self.cursor = byte;
        true
    }
    pub fn row(&self, width: usize) -> (usize, usize) {
        let (rows, (r, _)) = self.layout(width);
        (r, rows.len())
    }
}

// ---------------------------------------------------------------- history

pub struct History {
    path: std::path::PathBuf,
    entries: Vec<Value>,
    /// 0 = editing a fresh prompt; -n = n entries back.
    index: isize,
}

impl History {
    pub fn load() -> History {
        let path = crate::paths::get().state.join("prompt-history.jsonl");
        let entries: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("text").is_some_and(|t| t.is_string()) && v.get("pasted").is_some_and(|p| p.is_array()))
            .collect();
        let skip = entries.len().saturating_sub(50);
        History { path, entries: entries[skip..].to_vec(), index: 0 }
    }

    pub fn append(&mut self, text: &str, pasted: &[(String, String)], shell: bool) {
        let mut entry = json!({ "text": text, "files": [], "agents": [], "pasted": pasted.iter().map(|(p, full)| {
            let start = text.find(p.as_str()).unwrap_or(0);
            json!({ "text": full, "source": { "start": start, "end": start + p.chars().count(), "text": p } })
        }).collect::<Vec<_>>() });
        if shell {
            entry["mode"] = json!("shell");
        }
        self.index = 0;
        if self.entries.last() == Some(&entry) {
            return;
        }
        self.entries.push(entry);
        let skip = self.entries.len().saturating_sub(50);
        self.entries.drain(..skip);
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let body: String = self.entries.iter().map(|e| e.to_string() + "\n").collect();
        let _ = std::fs::write(&self.path, body);
    }

    /// `move`: returns (text, shell, pasted) for the entry `direction` away, or None.
    pub fn step(&mut self, direction: isize, current: &str) -> Option<(String, bool, Vec<(String, String)>)> {
        if self.entries.is_empty() {
            return None;
        }
        let cur = if self.index == 0 { None } else { self.entries.get((self.entries.len() as isize + self.index) as usize) };
        if let Some(c) = cur {
            if c["text"].as_str() != Some(current) && !current.is_empty() {
                return None;
            }
        } else if !current.is_empty() && self.index == 0 && direction < 0 {
            // upstream only browses from an empty prompt or an unedited entry
            return None;
        }
        let next = self.index + direction;
        if next.unsigned_abs() > self.entries.len() || next > 0 {
            return None;
        }
        self.index = next;
        if next == 0 {
            return Some((String::new(), false, vec![]));
        }
        let e = &self.entries[(self.entries.len() as isize + next) as usize];
        let pasted = e["pasted"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| Some((p["source"]["text"].as_str()?.to_string(), p["text"].as_str()?.to_string()))).collect())
            .unwrap_or_default();
        Some((e["text"].as_str().unwrap_or("").to_string(), e["mode"].as_str() == Some("shell"), pasted))
    }

    pub fn reset(&mut self) {
        self.index = 0;
    }
}

// ---------------------------------------------------------------- autocomplete

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoKind {
    Command,
    Reference,
}

#[derive(Clone, Debug)]
pub struct Option_ {
    pub display: String,
    pub description: String,
    pub label: &'static str,
    /// What selecting inserts or runs.
    pub value: String,
    pub action: Pick,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    /// Run a palette command by id.
    Command(String),
    /// Insert `/name ` and keep typing arguments.
    Slash(String),
    /// Replace the `@query` with `@value `.
    Mention(String),
}

pub struct Auto {
    pub kind: AutoKind,
    /// Byte offset of the trigger character.
    pub start: usize,
    pub selected: usize,
    pub options: Vec<Option_>,
}

/// fuzzysort-like score: all query chars in order, rewarding prefixes and contiguity.
pub fn fuzzy(hay: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let n: Vec<char> = needle.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut hi = 0;
    let mut last: Option<usize> = None;
    for c in &n {
        let found = (hi..h.len()).find(|i| h[*i] == *c)?;
        score += match last {
            Some(l) if l + 1 == found => 10,
            _ => 1,
        };
        if found == 0 || !h[found - 1].is_alphanumeric() {
            score += 5;
        }
        last = Some(found);
        hi = found + 1;
    }
    if h.iter().collect::<String>().starts_with(&n.iter().collect::<String>()) {
        score += 50;
    }
    Some(score - h.len() as i64 / 4)
}

/// Fuzzy-ranked workspace files (gitignore-aware), up to `limit`.
pub fn find_files(root: &std::path::Path, query: &str, limit: usize) -> Vec<(String, bool)> {
    let mut scored: Vec<(i64, String, bool)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(true).max_depth(Some(12)).build();
    for (n, e) in walker.flatten().enumerate() {
        if n > 20_000 {
            break;
        }
        let p = e.path();
        let Ok(rel) = p.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        let dir = e.file_type().is_some_and(|t| t.is_dir());
        if let Some(s) = fuzzy(&rel, query) {
            scored.push((s, if dir { format!("{rel}/") } else { rel }, dir));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored.into_iter().take(limit).map(|(_, p, d)| (p, d)).collect()
}

/// Detect an active trigger: `/` at the very start (no space yet) or `@word` before the cursor.
pub fn trigger(text: &str, cursor: usize) -> Option<(AutoKind, usize, String)> {
    let before = &text[..cursor];
    if let Some(rest) = before.strip_prefix('/') {
        if !rest.contains(char::is_whitespace) {
            return Some((AutoKind::Command, 0, rest.to_string()));
        }
    }
    let at = before.rfind('@')?;
    let q = &before[at + 1..];
    if q.contains(char::is_whitespace) {
        return None;
    }
    if at > 0 && !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    Some((AutoKind::Reference, at, q.to_string()))
}

pub fn paste_summary(text: &str) -> Option<String> {
    let content = text.trim();
    let lines = content.matches('\n').count() + 1;
    (lines >= 3 || content.chars().count() > 150).then(|| format!("[Pasted ~{lines} lines]"))
}

pub fn placeholder_width(term_width: u16) -> usize {
    let w = term_width as usize;
    if w < 44 {
        w.saturating_sub(5)
    } else {
        75usize.min(w.saturating_sub(4)).saturating_sub(5)
    }
}

pub fn placeholder(shell: bool, index: usize, term_width: u16) -> String {
    const NORMAL: [&str; 3] = ["Fix a TODO in the codebase", "What is the tech stack of this project?", "Fix broken tests"];
    const SHELL: [&str; 3] = ["ls -la", "git status", "pwd"];
    let v = if shell { format!("Run a command… \"{}\"", SHELL[index % 3]) } else { format!("Ask anything… \"{}\"", NORMAL[index % 3]) };
    draw::take_width(&v, placeholder_width(term_width).max(1)).trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.delete_word_back();
        assert_eq!(e.text, "hello ");
        e.undo();
        assert_eq!(e.text, "hello world");
        e.insert("\nsecond");
        assert!(e.vertical(40, false));
        let (rows, cur) = e.layout(4);
        // word wrap: an over-long word breaks by character, otherwise rows break after spaces
        assert_eq!(rows[0].1, "hell");
        assert_eq!(rows[1].1, "o ");
        assert_eq!(cur, (2, 0));
        let (rows, _) = e.layout(8);
        assert_eq!(rows.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(), ["hello ", "world", "second"]);
    }

    #[test]
    fn triggers() {
        assert_eq!(trigger("/mod", 4), Some((AutoKind::Command, 0, "mod".into())));
        assert_eq!(trigger("/models x", 9), None);
        assert_eq!(trigger("look @src/ma", 12), Some((AutoKind::Reference, 5, "src/ma".into())));
        assert_eq!(trigger("a@b", 3), None);
        assert_eq!(paste_summary("a\nb\nc").as_deref(), Some("[Pasted ~3 lines]"));
    }
}
