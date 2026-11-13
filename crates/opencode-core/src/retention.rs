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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_expired_files_only() {
        let dir = std::env::temp_dir().join(format!("opencode-retention-{}", crate::util::id("test")));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.txt");
        let fresh = dir.join("fresh.txt");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&fresh, "fresh").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        let times = std::fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60));
        file.set_times(times).unwrap();
        cleanup_dir(&dir, RETENTION);
        assert!(!old.exists());
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
