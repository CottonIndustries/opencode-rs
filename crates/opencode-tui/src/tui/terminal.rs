//! State for the `/terminal` dialog: a persistent interactive shell driven
//! through the same PTY manager the plugin `session.shell` API uses. Rendering
//! lives elsewhere; this module only owns the terminal and its screen buffer.

use anyhow::Result;
use ratatui::crossterm;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub struct TerminalDialog {
    id: String,
    title: String,
    lines: Vec<String>,
    cursor: Option<(u16, u16)>,
    cols: u16,
    rows: u16,
}

fn dim(screen: &Value, key: &str) -> Option<u16> {
    let value = screen.get(key)?.as_u64()?;
    Some(value.min(u16::MAX as u64) as u16)
}

/// Translate a key press into the bytes a terminal would send.
fn sequence(event: &crossterm::event::KeyEvent) -> Option<String> {
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = event.code {
            if c.is_ascii_alphabetic() {
                let byte = c.to_ascii_lowercase() as u8 & 0x1f;
                return Some(char::from(byte).to_string());
            }
        }
    }
    let data = match event.code {
        KeyCode::Char(c) => return Some(c.to_string()),
        KeyCode::Enter => "\r",
        KeyCode::Backspace => "\x7f",
        KeyCode::Tab => "\t",
        KeyCode::Delete => "\x1b[3~",
        KeyCode::Esc => "\x1b",
        KeyCode::Up => "\x1b[A",
        KeyCode::Down => "\x1b[B",
        KeyCode::Right => "\x1b[C",
        KeyCode::Left => "\x1b[D",
        KeyCode::Home => "\x1b[H",
        KeyCode::End => "\x1b[F",
        KeyCode::PageUp => "\x1b[5~",
        KeyCode::PageDown => "\x1b[6~",
        _ => return None,
    };
    Some(data.to_string())
}

impl TerminalDialog {
    pub fn open(session_id: &str, cwd: &Path, cols: u16, rows: u16) -> Result<Self> {
        // `Terminal::spawn` launches the detected shell (see
        // `crate::tools::shell::detect`) interactively; its `command` is only
        // typed into that shell, so an empty one opens a prompt without
        // starting a nested shell.
        let command = String::new();
        let args: Vec<String> = Vec::new();
        let environment = BTreeMap::new();
        crate::plugin::pty::manager().create(session_id, &command, &args, cwd, &environment, cols.max(1), rows.max(1))?;
        let mut dialog = Self {
            id: session_id.to_string(),
            title: String::new(),
            lines: Vec::new(),
            cursor: None,
            cols: cols.max(1),
            rows: rows.max(1),
        };
        dialog.refresh();
        Ok(dialog)
    }

    pub fn refresh(&mut self) {
        let Some(value) = crate::plugin::pty::manager().read(&self.id) else {
            self.lines.clear();
            self.cursor = None;
            return;
        };
        if let Some(title) = value.get("title").and_then(Value::as_str) {
            self.title = title.to_string();
        }
        let Some(screen) = value.get("screen") else { return };
        let text = screen.get("text").and_then(Value::as_str).unwrap_or_default();
        self.lines = text.lines().map(|line| line.trim_end().to_string()).collect();
        if let Some(cols) = dim(screen, "cols") {
            self.cols = cols;
        }
        if let Some(rows) = dim(screen, "rows") {
            self.rows = rows;
        }
        self.cursor = screen.get("cursor").and_then(|cursor| {
            let x = dim(cursor, "x")?;
            let y = dim(cursor, "y")?;
            Some((x, y))
        });
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        crate::plugin::pty::manager().resize(&self.id, cols, rows)?;
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.refresh();
        Ok(())
    }

    pub fn write(&self, data: &str) -> Result<()> {
        crate::plugin::pty::manager().write(&self.id, data)
    }

    pub fn key(&self, event: &crossterm::event::KeyEvent) -> Result<bool> {
        let Some(data) = sequence(event) else { return Ok(false) };
        self.write(&data)?;
        Ok(true)
    }

    /// End the shell. Only dialog close/quit should call this; a refresh, a
    /// resize or a keystroke keeps the terminal alive.
    pub fn close(&self) {
        crate::plugin::pty::manager().kill(&self.id);
    }
}

impl Drop for TerminalDialog {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> Option<String> {
        sequence(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Option<String> {
        sequence(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn keys_map_to_terminal_bytes() {
        assert_eq!(key(KeyCode::Char('a')).as_deref(), Some("a"));
        assert_eq!(key(KeyCode::Char('é')).as_deref(), Some("é"));
        assert_eq!(key(KeyCode::Enter).as_deref(), Some("\r"));
        assert_eq!(key(KeyCode::Backspace).as_deref(), Some("\x7f"));
        assert_eq!(key(KeyCode::Tab).as_deref(), Some("\t"));
        assert_eq!(key(KeyCode::Delete).as_deref(), Some("\x1b[3~"));
        assert_eq!(key(KeyCode::Esc).as_deref(), Some("\x1b"));
        assert_eq!(key(KeyCode::Up).as_deref(), Some("\x1b[A"));
        assert_eq!(key(KeyCode::Down).as_deref(), Some("\x1b[B"));
        assert_eq!(key(KeyCode::Right).as_deref(), Some("\x1b[C"));
        assert_eq!(key(KeyCode::Left).as_deref(), Some("\x1b[D"));
        assert_eq!(key(KeyCode::Home).as_deref(), Some("\x1b[H"));
        assert_eq!(key(KeyCode::End).as_deref(), Some("\x1b[F"));
        assert_eq!(key(KeyCode::PageUp).as_deref(), Some("\x1b[5~"));
        assert_eq!(key(KeyCode::PageDown).as_deref(), Some("\x1b[6~"));
        assert_eq!(key(KeyCode::F(5)), None);
        assert_eq!(ctrl('c').as_deref(), Some("\x03"));
        assert_eq!(ctrl('D').as_deref(), Some("\x04"));
    }
}
