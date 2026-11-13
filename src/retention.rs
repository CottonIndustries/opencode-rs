//! Time-based cleanup for disposable runtime files.

use std::path::Path;
use std::time::{Duration, SystemTime};

pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn cleanup_dir(directory: &Path, retention: Duration) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let cutoff = SystemTime::now().checked_sub(retention);
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };
        let modified = metadata.modified().ok();
        let expired = cutoff.is_some_and(|cutoff| modified.is_some_and(|modified| modified < cutoff));
        if !expired {
            continue;
        }
        if metadata.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn cleanup() {
    let paths = crate::paths::get();
    cleanup_dir(&paths.tool_output(), RETENTION);
    cleanup_dir(&paths.tmp.join("shell"), RETENTION);
}

pub fn start(app: std::sync::Arc<crate::app::App>) {
    cleanup();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
        interval.tick().await;
        loop {
            interval.tick().await;
            cleanup();
            let _ = &app;
        }
    });
}
