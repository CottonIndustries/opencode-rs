//! `opencode run`: non-interactive mode that streams one turn to the terminal.

use crate::app::{App, AppEvent, AskMode, PromptOptions};
use crate::session::{FilePart, Message, Part};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::json;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::sync::Arc;

pub struct RunArgs {
    pub message: Vec<String>,
    pub continue_last: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub format: String,
    pub files: Vec<String>,
    pub title: Option<String>,
    pub thinking: bool,
    pub auto_approve: bool,
}

/// Build message parts for an attached file or directory.
pub fn attach(path: &Path, display: &str) -> Result<Vec<Part>> {
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)?
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() { format!("{n}/") } else { n }
            })
            .collect();
        names.sort();
        return Ok(vec![Part::Text { text: format!("\n\nAttached directory: {}\n\n{}", path.display(), names.join("\n")), synthetic: true }]);
    }
    let bytes = std::fs::read(path).map_err(|e| anyhow!("cannot read {display}: {e}"))?;
    let mime = mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string();
    if mime.starts_with("image/") || mime == "application/pdf" {
        return Ok(vec![
            Part::Text { text: format!("Attached file: {}", path.display()), synthetic: true },
            Part::File(FilePart { mime, data: base64::engine::general_purpose::STANDARD.encode(&bytes), name: display.to_string() }),
        ]);
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(vec![Part::Text { text: format!("\n\nAttached file: {}\n\n{}", path.display(), text), synthetic: true }])
}

/// Expand `@path` mentions into attachments.
pub fn expand_mentions(app: &App, text: &str) -> Vec<Part> {
    let re = regex::Regex::new(r"(?:^|\s)@([^\s]+)").unwrap();
    let mut parts = Vec::new();
    for c in re.captures_iter(text) {
        let raw = c[1].trim_end_matches([',', '.', ')', ';', ':']);
        let p = crate::util::resolve(&app.directory, raw);
        if p.exists() {
            if let Ok(mut a) = attach(&p, raw) {
                parts.append(&mut a);
            }
        }
    }
    parts
}

/// Read piped stdin. When a message was already given, an open pipe that
/// produces nothing (e.g. an inherited CI stdin) is ignored instead of blocking.
fn read_piped_stdin(optional: bool) -> String {
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut signalled = false;
        loop {
            match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !signalled {
                        signalled = true;
                        let _ = started_tx.send(());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
        let _ = started_tx.send(());
        let _ = done_tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    if optional && started_rx.recv_timeout(std::time::Duration::from_millis(300)).is_err() {
        return String::new();
    }
    done_rx.recv().unwrap_or_default()
}

fn latest_root_session(app: &App) -> Option<String> {
    crate::session::list(&app.project).into_iter().find(|s| s.parent_id.is_none()).map(|s| s.id)
}

pub async fn run(app: Arc<App>, args: RunArgs) -> Result<()> {
    *app.ask_mode.lock().unwrap() = if args.auto_approve { AskMode::AutoApprove } else { AskMode::AutoReject };
    let mut text = args.message.join(" ");
    if !std::io::stdin().is_terminal() {
        let input = read_piped_stdin(!text.is_empty());
        if !input.trim().is_empty() {
            text = if text.is_empty() { input } else { format!("{text}\n\n{input}") };
        }
    }
    // a leading /command expands its template (plugin commands run later,
    // once their session exists)
    let mut plugin_command: Option<(String, String)> = None;
    if let Some(rest) = text.strip_prefix('/') {
        let (name, cmd_args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if app.plugin_commands().contains(name) {
            plugin_command = Some((name.to_string(), cmd_args.trim().to_string()));
            text.clear();
        } else if let Some(cmd) = app.commands().get(name) {
            text = crate::command::render(cmd, cmd_args.trim(), &app.directory, &app.worktree);
        }
    }
    if text.trim().is_empty() && args.files.is_empty() && plugin_command.is_none() {
        return Err(anyhow!("You must provide a message or a command"));
    }

    let handle = if let Some(id) = &args.session {
        match app.session(id) {
            Ok(h) => h,
            Err(_) => {
                let h = app.create_session(None);
                h.with(|s| s.id = id.clone());
                h.save();
                app.session(id)?
            }
        }
    } else if args.continue_last {
        match latest_root_session(&app) {
            Some(id) => app.session(&id)?,
            None => app.create_session(None),
        }
    } else {
        app.create_session(None)
    };
    let handle = if args.fork && (args.session.is_some() || args.continue_last) {
        let forked = crate::session::fork(&handle.session.lock().unwrap());
        crate::session::save(&forked)?;
        app.session(&forked.id)?
    } else {
        handle
    };
    if let Some(t) = &args.title {
        handle.with(|s| s.title = t.clone());
        handle.save();
    }
    let sid = handle.id();

    if let Some((name, cmd_args)) = plugin_command {
        let mut rx = app.subscribe();
        let ack = app.plugins.execute_command(&name, &sid, &cmd_args).await?;
        let admitted = ack
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| matches!(kind, "user" | "synthetic" | "compaction"));
        // The command may call session.prompt; wait for that turn to settle.
        let started = tokio::time::Instant::now();
        let mut saw_busy = app.session(&sid).map(|h| h.is_busy()).unwrap_or(false);
        let deadline = started + std::time::Duration::from_secs(600);
        loop {
            if saw_busy && !app.session(&sid).map(|h| h.is_busy()).unwrap_or(false) {
                break;
            }
            // A command that only performs host-side work never starts a turn.
            if !saw_busy && started.elapsed() > std::time::Duration::from_secs(if admitted { 10 } else { 3 }) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(AppEvent::Busy { session, busy })) if session == sid => {
                    if busy {
                        saw_busy = true;
                    } else if saw_busy {
                        break;
                    }
                }
                Ok(Ok(AppEvent::TextDelta { delta, .. })) => {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                Ok(Ok(AppEvent::Error { message, .. })) => {
                    eprintln!("{message}");
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {
                    if !app.session(&sid).map(|h| h.is_busy()).unwrap_or(false) && saw_busy {
                        break;
                    }
                }
            }
        }
        println!();
        app.plugins.flush().await;
        app.plugins.shutdown().await;
        return Ok(());
    }

    let mut msg = Message::user_text(text.clone());
    msg.parts.extend(expand_mentions(&app, &text));
    for f in &args.files {
        let p = crate::util::resolve(&app.directory, f);
        msg.parts.extend(attach(&p, f)?);
    }

    let json_mode = args.format == "json";
    let mut rx = app.subscribe();
    let opts = PromptOptions { agent: args.agent.clone(), model: args.model.clone(), variant: None };
    let app2 = app.clone();
    let sid2 = sid.clone();
    let mut task = tokio::spawn(async move { crate::engine::prompt(app2, &sid2, msg, opts).await });

    let color = std::io::stderr().is_terminal();
    let dim = |s: &str| if color { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() };
    let mut out = std::io::stdout();
    let mut in_reasoning = false;
    let mut last_was_text = false;
    let mut had_error = false;
    let emit_json = |v: serde_json::Value| {
        println!("{v}");
    };
    let mut handle_event = |ev: AppEvent, out: &mut std::io::Stdout| {
        if ev.session() != Some(sid.as_str()) {
            return;
        }
        match ev {
            AppEvent::TextDelta { delta, .. } => {
                if json_mode {
                    emit_json(json!({ "type": "text", "sessionID": sid, "text": delta }));
                    return;
                }
                if in_reasoning {
                    in_reasoning = false;
                    eprintln!();
                }
                let _ = write!(out, "{delta}");
                let _ = out.flush();
                last_was_text = true;
            }
            AppEvent::ReasoningDelta { delta, .. } => {
                if json_mode {
                    if args.thinking {
                        emit_json(json!({ "type": "reasoning", "sessionID": sid, "text": delta }));
                    }
                    return;
                }
                if args.thinking {
                    if !in_reasoning {
                        in_reasoning = true;
                        eprint!("{}", dim("Thinking: "));
                    }
                    eprint!("{}", dim(&delta));
                }
            }
            AppEvent::ToolStarted { id, name, input, .. } => {
                if json_mode {
                    emit_json(json!({ "type": "tool_start", "sessionID": sid, "id": id, "tool": name, "input": input }));
                    return;
                }
                if last_was_text {
                    println!();
                    last_was_text = false;
                }
                if in_reasoning {
                    in_reasoning = false;
                    eprintln!();
                }
                let summary = crate::tui::render::tool_summary(&name, &input);
                eprintln!("{} {}", if color { format!("\x1b[36m│ {name}\x1b[0m") } else { format!("| {name}") }, summary);
            }
            AppEvent::ToolFinished { id, name, title, output, error, .. } => {
                if json_mode {
                    emit_json(json!({ "type": "tool_end", "sessionID": sid, "id": id, "tool": name, "title": title, "output": output, "error": error }));
                    return;
                }
                if let Some(e) = error {
                    eprintln!("{}", if color { format!("\x1b[31m  ✗ {}\x1b[0m", crate::util::first_line(&e)) } else { format!("  x {}", crate::util::first_line(&e)) });
                }
            }
            AppEvent::Retry { message, .. } => {
                if json_mode {
                    emit_json(json!({ "type": "retry", "sessionID": sid, "message": message }));
                } else {
                    eprintln!("{}", dim(&format!("retry: {message}")));
                }
            }
            AppEvent::Error { message, .. } => {
                had_error = true;
                if json_mode {
                    emit_json(json!({ "type": "error", "sessionID": sid, "message": message }));
                } else {
                    eprintln!("{}", if color { format!("\x1b[31mError: {message}\x1b[0m") } else { format!("Error: {message}") });
                }
            }
            AppEvent::StepFinished { .. } if json_mode => emit_json(json!({ "type": "step_finish", "sessionID": sid })),
            _ => {}
        }
    };
    let result = loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => handle_event(ev, &mut out),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => {}
            },
            r = &mut task => break r,
            _ = tokio::signal::ctrl_c() => {
                handle.abort();
            }
        }
    };
    let mut title_done = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(&ev, AppEvent::Title { session, .. } if *session == sid) {
            title_done = true;
        }
        handle_event(ev, &mut out);
    }
    // let the background title request finish so the session is findable later
    let titling = handle.with(|s| s.has_default_title() && s.parent_id.is_none() && s.messages.iter().filter(|m| m.role == crate::session::Role::User).count() == 1);
    if titling && !title_done && args.title.is_none() {
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(AppEvent::Title { session, .. }) if session == sid => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), wait).await;
    }
    if !json_mode {
        println!();
    }
    let result = result.map_err(|e| anyhow!("{e}"))?;
    if json_mode {
        let (usage, cost) = handle.with(|s| s.usage_total());
        println!("{}", json!({ "type": "done", "sessionID": sid, "usage": usage, "cost": cost }));
    }
    result?;
    app.plugins.flush().await;
    app.plugins.shutdown().await;
    if had_error {
        std::process::exit(1);
    }
    Ok(())
}
