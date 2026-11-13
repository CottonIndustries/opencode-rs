//! Detached local server lifecycle for `opencode service`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn state_path() -> PathBuf {
    crate::paths::get().storage().join("server.json")
}

fn log_path() -> PathBuf {
    crate::paths::get().log.join("service.log")
}

fn read_state() -> Option<Value> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_state(state: &Value) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        return output.is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|value| value == pid.to_string())
        });
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn kill(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

pub fn status() -> Value {
    let Some(state) = read_state() else {
        return json!({ "running": false });
    };
    let pid = state.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
    let running = pid != 0 && alive(pid);
    json!({
        "running": running,
        "pid": pid,
        "port": state.get("port").cloned().unwrap_or(Value::Null),
        "directory": state.get("directory").cloned().unwrap_or(Value::Null),
        "url": state.get("url").cloned().unwrap_or(Value::Null),
        "log": log_path().to_string_lossy(),
    })
}

pub fn start(port: u16, directory: PathBuf) -> Result<Value> {
    let current = status();
    if current.get("running").and_then(Value::as_bool) == Some(true) {
        return Err(anyhow!(
            "server already running at {}",
            current.get("url").and_then(Value::as_str).unwrap_or("unknown")
        ));
    }
    crate::paths::get().ensure();
    let directory = std::fs::canonicalize(&directory)
        .map(|path| crate::app::strip_unc(&path))
        .unwrap_or(directory);
    let executable = std::env::current_exe()?;
    let log = log_path();
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let error = output.try_clone()?;
    let mut command = Command::new(executable);
    command
        .args(["serve", "--port", &port.to_string(), "--dir"])
        .arg(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    let child = command.spawn()?;
    let state = json!({
        "pid": child.id(),
        "port": port,
        "directory": directory.to_string_lossy(),
        "url": format!("http://127.0.0.1:{port}"),
        "log": log.to_string_lossy(),
        "started": crate::util::now_ms(),
    });
    save_state(&state)?;
    Ok(state)
}

pub fn stop() -> Result<Value> {
    let Some(state) = read_state() else {
        return Ok(json!({ "stopped": false }));
    };
    let pid = state.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
    let stopped = pid != 0 && kill(pid);
    let _ = std::fs::remove_file(state_path());
    Ok(json!({ "stopped": stopped, "pid": pid }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_without_state_is_stopped() {
        let _ = std::fs::remove_file(state_path());
        assert_eq!(status()["running"], false);
    }
}
