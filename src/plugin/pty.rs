//! Persistent terminals for the plugin `session.shell` and
//! `experimental.terminal.read` APIs.

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

const ROWS: u16 = 40;
const COLS: u16 = 120;

struct Terminal {
    cwd: String,
    rows: u16,
    cols: u16,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

pub struct Terminals {
    sessions: Mutex<HashMap<String, Terminal>>,
}

static TERMINALS: OnceLock<Terminals> = OnceLock::new();

pub fn manager() -> &'static Terminals {
    TERMINALS.get_or_init(|| Terminals { sessions: Mutex::new(HashMap::new()) })
}

impl Terminals {
    /// Spawn an interactive shell for the session, or send the command to the
    /// existing shell.
    pub fn shell(
        &self,
        session_id: &str,
        command: &str,
        cwd: &Path,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(terminal) = sessions.get_mut(session_id) {
            return terminal.write(command);
        }
        if command.is_empty() {
            return Ok(());
        }
        let terminal = Terminal::spawn(command, cwd, environment)?;
        sessions.insert(session_id.to_string(), terminal);
        Ok(())
    }

    pub fn read(&self, session_id: &str) -> Option<Value> {
        let sessions = self.sessions.lock().unwrap();
        let terminal = sessions.get(session_id)?;
        let parser = terminal.parser.lock().unwrap();
        let screen = parser.screen();
        let (row, col) = screen.cursor_position();
        Some(json!({
            "ptyID": session_id,
            "title": screen.title(),
            "cwd": terminal.cwd,
            "foregroundProcess": Value::Null,
            "screen": {
                "text": screen.contents(),
                "cols": terminal.cols,
                "rows": terminal.rows,
                "cursor": { "x": col, "y": row },
            },
        }))
    }

    pub fn kill(&self, session_id: &str) {
        if let Some(terminal) = self.sessions.lock().unwrap().remove(session_id) {
            let _ = terminal.child.lock().unwrap().kill();
        }
    }
}

impl Terminal {
    fn spawn(
        command: &str,
        cwd: &Path,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        let shell = crate::tools::shell::detect();
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| anyhow!("opening terminal failed: {error}"))?;
        // The interactive shell must not receive the non-interactive `-c`
        // arguments used by the shell tool.
        let mut builder = CommandBuilder::new(&shell.program);
        let name = shell
            .program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.contains("bash") || name == "sh" || name == "zsh" {
            builder.arg("-i");
        } else if name.contains("pwsh") || name.contains("powershell") {
            builder.arg("-NoLogo");
        }
        builder.cwd(cwd);
        builder.env("TERM", "xterm-256color");
        for (key, value) in environment {
            builder.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| anyhow!("starting terminal shell failed: {error}"))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| anyhow!("terminal reader failed: {error}"))?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|error| anyhow!("terminal writer failed: {error}"))?,
        ));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 1000)));
        let sink = parser.clone();
        let replies = writer.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        // ConPTY asks the terminal for the cursor position
                        // before it will render output.
                        if buffer[..read].windows(4).any(|bytes| bytes == b"\x1b[6n") {
                            if let Ok(mut writer) = replies.lock() {
                                let _ = writer.write_all(b"\x1b[1;1R");
                                let _ = writer.flush();
                            }
                        }
                        if let Ok(mut parser) = sink.lock() {
                            parser.process(&buffer[..read]);
                        }
                    }
                }
            }
        });
        let mut terminal = Terminal {
            cwd: cwd.to_string_lossy().into_owned(),
            rows: ROWS,
            cols: COLS,
            parser,
            writer,
            _master: Mutex::new(pair.master),
            child: Mutex::new(child),
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        terminal.write(command)?;
        Ok(terminal)
    }

    fn write(&mut self, command: &str) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(command.as_bytes())?;
        writer.write_all(b"\r\n")?;
        writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_an_unknown_terminal_is_none() {
        assert!(manager().read("missing").is_none());
    }


}
