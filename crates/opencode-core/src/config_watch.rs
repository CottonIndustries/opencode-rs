//! Configuration file watcher.
//!
//! Watches the files loaded for this instance and emits a notice when they
//! change so the UI can prompt for a restart. Reloading every registry in
//! place while turns are running is intentionally avoided.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

fn stamps(files: &[PathBuf]) -> BTreeMap<PathBuf, Option<SystemTime>> {
    files
        .iter()
        .map(|path| (path.clone(), std::fs::metadata(path).and_then(|meta| meta.modified()).ok()))
        .collect()
}

pub fn start(app: std::sync::Arc<crate::app::App>) {
    if app.config_files.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut previous = stamps(&app.config_files);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let current = stamps(&app.config_files);
            if current != previous {
                previous = current;
                match app.reload_config() {
                    Ok(()) => app.emit(crate::app::AppEvent::Notice {
                        message: "Configuration reloaded.".into(),
                    }),
                    Err(error) => app.emit(crate::app::AppEvent::Notice {
                        message: format!("Configuration changed: {error}"),
                    }),
                }
            }
        }
    });
}
