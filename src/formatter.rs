//! Configurable post-edit file formatters.

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub async fn format(app: &crate::app::App, file: &Path) -> Result<bool> {
    let config = app.config();
    let Some(Value::Object(formatters)) = config.formatter.as_ref() else {
        return Ok(false);
    };
    let extension = file.extension().and_then(|value| value.to_str()).map(|value| format!(".{value}"));
    for (name, config) in formatters {
        if config.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let extensions = config
            .get("extensions")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>());
        if let (Some(extensions), Some(extension)) = (&extensions, &extension) {
            if !extensions.iter().any(|item| item == extension) {
                continue;
            }
        }
        let command = config
            .get("command")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![name.clone(), "$FILE".into()]);
        if command.is_empty() {
            continue;
        }
        let cwd = file.parent().map(Path::to_path_buf).unwrap_or_else(|| app.directory.clone());
        let file = file.to_string_lossy().to_string();
        let mut process = tokio::process::Command::new(&command[0]);
        process
            .args(command.iter().skip(1).map(|argument| argument.replace("$FILE", &file)))
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(environment) = config.get("environment").and_then(Value::as_object) {
            for (key, value) in environment {
                if let Some(value) = value.as_str() {
                    process.env(key, value);
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x0800_0000);
        }
        if process.status().await.is_ok_and(|status| status.success()) {
            return Ok(true);
        }
    }
    Ok(false)
}
