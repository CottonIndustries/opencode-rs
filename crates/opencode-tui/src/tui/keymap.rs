//! Key bindings with upstream's syntax and defaults (`config/keybind.ts`):
//! comma-separated alternatives, `<leader>` sequences and `none`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub leader: bool,
    pub key: Key,
}

pub const DEFAULTS: &[(&str, &str)] = &[
    ("leader", "ctrl+x"),
    ("app.exit", "ctrl+c,ctrl+d,<leader>q"),
    ("command.palette.show", "ctrl+p"),
    ("help.show", "none"),
    ("prompt.editor", "<leader>e"),
    ("prompt.skills", "none"),
    ("theme.switch", "none"),
    ("session.sidebar.toggle", "<leader>b"),
    ("opencode.status", "<leader>s"),
    ("session.export", "<leader>x"),
    ("session.copy", "none"),
    ("session.new", "<leader>n"),
    ("session.list", "<leader>l"),
    ("session.tab.next", "ctrl+tab,alt+down"),
    ("session.tab.previous", "ctrl+shift+tab,alt+up"),
    ("session.tab.close", "<leader>w"),
    ("session.tab.reopen", "ctrl+shift+t"),
    ("session.tab.select.1", "<leader>1,ctrl+1"),
    ("session.tab.select.2", "<leader>2,ctrl+2"),
    ("session.tab.select.3", "<leader>3,ctrl+3"),
    ("session.tab.select.4", "<leader>4,ctrl+4"),
    ("session.tab.select.5", "<leader>5,ctrl+5"),
    ("session.tab.select.6", "<leader>6,ctrl+6"),
    ("session.tab.select.7", "<leader>7,ctrl+7"),
    ("session.tab.select.8", "<leader>8,ctrl+8"),
    ("session.tab.select.9", "<leader>9,ctrl+9"),
    ("session.tab.select.10", "<leader>0,ctrl+0"),
    ("session.tab.history.back", "none"),
    ("session.tab.history.forward", "none"),
    ("session.tab.next_unread", "alt+shift+down"),
    ("session.tab.previous_unread", "alt+shift+up"),
    ("session.timeline", "<leader>g"),
    ("session.fork", "none"),
    ("session.rename", "ctrl+r"),
    ("session.delete", "ctrl+d"),
    ("session.interrupt", "escape"),
    ("session.compact", "<leader>c"),
    ("terminal.toggle", "<leader>t"),
    ("terminal.select", "<leader>down"),
    ("terminal.close", "<leader>up"),
    ("session.toggle.scrollbar", "none"),
    ("session.toggle.thinking", "none"),
    ("session.toggle.exploration_grouping", "none"),
    ("session.verbosity.cycle", "none"),
    ("model.dialog.provider", "ctrl+a"),
    ("model.dialog.favorite", "ctrl+f"),
    ("model.list", "<leader>m"),
    ("model.cycle_recent", "f2"),
    ("model.cycle_recent_reverse", "shift+f2"),
    ("model.cycle_favorite", "none"),
    ("model.cycle_favorite_reverse", "none"),
    ("mcp.list", "none"),
    ("agent.list", "<leader>a"),
    ("agent.cycle", "shift+tab"),
    ("agent.cycle.reverse", "none"),
    ("variant.cycle", "ctrl+t"),
    ("variant.list", "none"),
    ("session.page.up", "pageup,ctrl+alt+b"),
    ("session.page.down", "pagedown,ctrl+alt+f"),
    ("session.line.up", "ctrl+alt+y"),
    ("session.line.down", "ctrl+alt+e"),
    ("session.half.page.up", "ctrl+alt+u"),
    ("session.half.page.down", "ctrl+alt+d"),
    ("session.first", "ctrl+g,home,alt+home"),
    ("session.last", "ctrl+alt+g,end"),
    ("session.messages_last_user", "alt+end"),
    ("messages.copy", "<leader>y"),
    ("session.undo", "<leader>u"),
    ("session.redo", "<leader>r"),
    ("session.background", "ctrl+b"),
    ("session.child.first", "down"),
    ("prompt.clear", "ctrl+c"),
    ("prompt.paste", "ctrl+v"),
    ("input.submit", "return"),
    ("input.newline", "shift+return,ctrl+return,alt+return,ctrl+j"),
    ("input.move.left", "left,ctrl+b"),
    ("input.move.right", "right,ctrl+f"),
    ("input.move.up", "up"),
    ("input.move.down", "down"),
    ("input.line.home", "ctrl+a"),
    ("input.line.end", "ctrl+e"),
    ("input.visual.line.home", "alt+a"),
    ("input.visual.line.end", "alt+e"),
    ("input.delete.line", "ctrl+shift+d"),
    ("input.delete.to.line.end", "ctrl+k"),
    ("input.delete.to.line.start", "ctrl+u"),
    ("input.backspace", "backspace,shift+backspace"),
    ("input.delete", "ctrl+d,delete,shift+delete"),
    ("input.undo", "ctrl+-,super+z"),
    ("input.redo", "ctrl+.,super+shift+z"),
    ("input.word.forward", "alt+f,alt+right,ctrl+right"),
    ("input.word.backward", "alt+b,alt+left,ctrl+left"),
    ("input.delete.word.forward", "alt+d,alt+delete,ctrl+delete"),
    ("input.delete.word.backward", "ctrl+w,ctrl+backspace,alt+backspace"),
    ("prompt.history.previous", "up"),
    ("prompt.history.next", "down"),
    ("dialog.select.prev", "up,ctrl+p"),
    ("dialog.select.next", "down,ctrl+n"),
    ("dialog.select.page_up", "pageup"),
    ("dialog.select.page_down", "pagedown"),
    ("dialog.select.home", "home"),
    ("dialog.select.end", "end"),
    ("dialog.select.submit", "return"),
    ("prompt.autocomplete.prev", "up,ctrl+p"),
    ("prompt.autocomplete.next", "down,ctrl+n"),
    ("prompt.autocomplete.hide", "escape"),
    ("prompt.autocomplete.select", "return"),
    ("prompt.autocomplete.complete", "tab"),
    ("permission.prompt.fullscreen", "ctrl+f"),
    ("terminal.suspend", "ctrl+z"),
];

pub struct Keymap {
    binds: HashMap<String, Vec<Binding>>,
    leader: Option<Key>,
}

fn parse_key(s: &str) -> Option<Key> {
    let s = s.trim().to_lowercase();
    if s.is_empty() || s == "none" {
        return None;
    }
    // "ctrl+-" and "ctrl++" keep their trailing symbol
    let (mods, name) = match s.rfind('+') {
        Some(i) if i + 1 == s.len() && i > 0 => (&s[..i - 1], "+".to_string()),
        Some(i) => (&s[..i], s[i + 1..].to_string()),
        None => ("", s.clone()),
    };
    let mut k = Key { ctrl: false, alt: false, shift: false, name };
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        match m {
            "ctrl" | "control" => k.ctrl = true,
            "alt" | "meta" | "option" => k.alt = true,
            "shift" => k.shift = true,
            "super" | "cmd" => return None,
            _ => {}
        }
    }
    k.name = match k.name.as_str() {
        "enter" => "return".into(),
        "esc" => "escape".into(),
        "kppageup" => "pageup".into(),
        "kppagedown" => "pagedown".into(),
        "kphome" => "home".into(),
        "kpend" => "end".into(),
        other => other.to_string(),
    };
    Some(k)
}

pub fn parse(value: &str) -> Vec<Binding> {
    value
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            match part.strip_prefix("<leader>") {
                Some(rest) => parse_key(rest).map(|key| Binding { leader: true, key }),
                None => parse_key(part).map(|key| Binding { leader: false, key }),
            }
        })
        .collect()
}

fn key_label(k: &Key) -> String {
    let mut s = String::new();
    if k.ctrl {
        s.push_str("ctrl+");
    }
    if k.alt {
        s.push_str("alt+");
    }
    if k.shift {
        s.push_str("shift+");
    }
    s.push_str(&k.name);
    s
}

/// Normalize a terminal key event into the binding vocabulary.
pub fn from_event(ev: &KeyEvent) -> Option<Key> {
    let m = ev.modifiers;
    let mut k = Key { ctrl: m.contains(KeyModifiers::CONTROL), alt: m.contains(KeyModifiers::ALT), shift: m.contains(KeyModifiers::SHIFT), name: String::new() };
    k.name = match ev.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                k.shift = true;
            }
            if c == ' ' {
                "space".into()
            } else if k.ctrl && (c as u32) < 32 {
                // some terminals report ctrl+letter as the raw control character
                char::from_u32(c as u32 + 96).map(|c| c.to_string())?
            } else {
                let lower = c.to_lowercase().to_string();
                // shifted symbols arrive as the symbol itself
                if !c.is_alphabetic() {
                    k.shift = false;
                }
                lower
            }
        }
        KeyCode::Enter => "return".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => {
            k.shift = true;
            "tab".into()
        }
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    };
    Some(k)
}

impl Keymap {
    pub fn new(overrides: &serde_json::Value) -> Keymap {
        let mut binds = HashMap::new();
        for (name, def) in DEFAULTS {
            let v = overrides.get(*name).and_then(|v| v.as_str()).unwrap_or(def);
            binds.insert(name.to_string(), parse(v));
        }
        let leader = binds.get("leader").and_then(|b| b.first()).map(|b| b.key.clone());
        Keymap { binds, leader }
    }

    pub fn is_leader(&self, k: &Key) -> bool {
        self.leader.as_ref() == Some(k)
    }

    /// Does `k` (pressed after the leader when `leader` is set) trigger `command`?
    pub fn matches(&self, command: &str, k: &Key, leader: bool) -> bool {
        self.binds.get(command).is_some_and(|bs| bs.iter().any(|b| b.leader == leader && b.key == *k))
    }

    /// First shortcut for display: `ctrl+x m`.
    pub fn label(&self, command: &str) -> Option<String> {
        let b = self.binds.get(command)?.first()?;
        Some(self.binding_label(b))
    }

    fn binding_label(&self, b: &Binding) -> String {
        if b.leader {
            let leader = self.leader.as_ref().map(key_label).unwrap_or_else(|| "ctrl+x".into());
            format!("{leader} {}", key_label(&b.key))
        } else {
            key_label(&b.key)
        }
    }

    /// All shortcuts for display, comma separated.
    pub fn labels(&self, command: &str) -> Option<String> {
        let bs = self.binds.get(command)?;
        if bs.is_empty() {
            return None;
        }
        Some(bs.iter().map(|b| self.binding_label(b)).collect::<Vec<_>>().join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_matching() {
        let km = Keymap::new(&serde_json::json!({}));
        assert_eq!(km.label("model.list").as_deref(), Some("ctrl+x m"));
        assert_eq!(km.label("agent.cycle").as_deref(), Some("shift+tab"));
        assert_eq!(km.label("command.palette.show").as_deref(), Some("ctrl+p"));
        let ev = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(km.matches("agent.cycle", &from_event(&ev).unwrap(), false));
        let ev = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(km.is_leader(&from_event(&ev).unwrap()));
        let km = Keymap::new(&serde_json::json!({ "model.list": "ctrl+o" }));
        assert_eq!(km.label("model.list").as_deref(), Some("ctrl+o"));
    }
}
