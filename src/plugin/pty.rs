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
    command: String,
    cwd: String,
    rows: u16,
    cols: u16,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: tokio::sync::broadcast::Sender<Vec<u8>>,
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
        let terminal = Terminal::spawn(command, cwd, environment, ROWS, COLS)?;
        sessions.insert(session_id.to_string(), terminal);
        Ok(())
    }

    pub fn create(
        &self,
        session_id: &str,
        command: &str,
        args: &[String],
        cwd: &Path,
        environment: &std::collections::BTreeMap<String, String>,
        cols: u16,
        rows: u16,
    ) -> Result<Value> {
        let mut line = command.to_string();
        for arg in args {
            line.push(' ');
            line.push_str(arg);
        }
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(existing) = sessions.remove(session_id) {
            let _ = existing.child.lock().unwrap().kill();
        }
        let terminal = Terminal::spawn(&line, cwd, environment, rows.max(1), cols.max(1))?;
        let info = terminal.info(session_id);
        sessions.insert(session_id.to_string(), terminal);
        Ok(info)
    }

    pub fn list(&self) -> Vec<Value> {
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().map(|(id, terminal)| terminal.info(id)).collect()
    }

    pub fn write(&self, session_id: &str, data: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let terminal = sessions.get_mut(session_id).ok_or_else(|| anyhow!("terminal not found: {session_id}"))?;
        terminal.write_raw(data)
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let sessions = self.sessions.lock().unwrap();
        let terminal = sessions.get(session_id).ok_or_else(|| anyhow!("terminal not found: {session_id}"))?;
        terminal.resize(cols.max(1), rows.max(1))
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

    pub fn subscribe(&self, session_id: &str) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|terminal| terminal.output.subscribe())
    }

    pub fn exists(&self, session_id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(session_id)
    }

    pub fn kill(&self, session_id: &str) {
        if let Some(terminal) = self.sessions.lock().unwrap().remove(session_id) {
            let _ = terminal.child.lock().unwrap().kill();
        }
    }
}

impl Terminal {
    fn info(&self, id: &str) -> Value {
        json!({
            "id": id,
            "title": self.parser.lock().unwrap().screen().title(),
            "command": self.command,
            "args": [],
            "cwd": self.cwd,
            "status": "running",
            "sessionID": id,
            "size": { "cols": self.cols, "rows": self.rows },
        })
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self._master
            .lock()
            .unwrap()
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| anyhow!("terminal resize failed: {error}"))?;
        self.parser.lock().unwrap().set_size(rows, cols);
        Ok(())
    }

    fn spawn(
        command: &str,
        cwd: &Path,
        environment: &std::collections::BTreeMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let shell = crate::tools::shell::detect();
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
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
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let sink = parser.clone();
        let replies = writer.clone();
        let (output, _) = tokio::sync::broadcast::channel(1024);
        let output_sink = output.clone();
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
                        let _ = output_sink.send(buffer[..read].to_vec());
                        if let Ok(mut parser) = sink.lock() {
                            parser.process(&buffer[..read]);
                        }
                    }
                }
            }
        });
        let mut terminal = Terminal {
            command: command.to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            rows,
            cols,
            parser,
            writer,
            output,
            _master: Mutex::new(pair.master),
            child: Mutex::new(child),
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        terminal.write(command)?;
        Ok(terminal)
    }

    fn write(&mut self, command: &str) -> Result<()> {
        self.write_raw(command)?;
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(b"\r\n")?;
        writer.flush()?;
        Ok(())
    }

    fn write_raw(&mut self, data: &str) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data.as_bytes())?;
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
