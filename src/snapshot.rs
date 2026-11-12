//! Working-tree snapshots used by session revert.
//!
//! Tracked files are captured with `git stash create`, which records the
//! working tree without changing the index or creating a stash entry. Untracked
//! files are copied into the snapshot directory. Restoring checks the tracked
//! tree back out and reapplies the untracked files.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_repo(directory: &Path) -> bool {
    git(directory, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

fn storage() -> PathBuf {
    crate::paths::get().storage().join("snapshots")
}

pub struct Snapshot {
    pub id: String,
    pub patch: String,
}

/// Capture the current working tree. Returns `None` outside git.
pub fn capture(directory: &Path) -> Option<Snapshot> {
    if !is_repo(directory) {
        return None;
    }
    let id = crate::util::id("snap");
    let root = storage().join(&id);
    std::fs::create_dir_all(&root).ok()?;
    let stash = git(directory, &["stash", "create"]).filter(|sha| !sha.is_empty());
    let head = git(directory, &["rev-parse", "HEAD"]);
    let patch = stash.or(head.clone()).unwrap_or_default();
    let untracked = git(directory, &["ls-files", "--others", "--exclude-standard", "-z"])
        .map(|list| {
            list.split('\0')
                .filter(|file| !file.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let files = root.join("files");
    for file in &untracked {
        let source = directory.join(file);
        let target = files.join(file);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&source, &target);
    }
    let meta = json!({
        "id": id,
        "patch": patch,
        "head": head,
        "untracked": untracked,
    });
    std::fs::write(root.join("meta.json"), serde_json::to_vec(&meta).ok()?).ok()?;
    Some(Snapshot { id, patch })
}

fn load(id: &str) -> Result<(Value, PathBuf)> {
    let root = storage().join(id);
    let meta = std::fs::read(root.join("meta.json"))?;
    let meta: Value = serde_json::from_slice(&meta)?;
    Ok((meta, root))
}

/// Restore the tracked tree and untracked files captured in `id`.
pub fn restore(directory: &Path, id: &str) -> Result<()> {
    let (meta, root) = load(id)?;
    if !is_repo(directory) {
        return Err(anyhow!("cannot restore a snapshot outside a git repository"));
    }
    let patch = meta.get("patch").and_then(Value::as_str).unwrap_or_default();
    if !patch.is_empty() {
        let mut cmd = Command::new("git");
        cmd.args(["checkout", patch, "--", "."])
            .current_dir(directory)
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    let previous: Vec<String> = meta
        .get("untracked")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let current = git(directory, &["ls-files", "--others", "--exclude-standard", "-z"])
        .map(|list| {
            list.split('\0')
                .filter(|file| !file.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for file in current {
        if !previous.contains(&file) {
            let _ = std::fs::remove_file(directory.join(&file));
        }
    }
    for file in previous {
        let source = root.join("files").join(&file);
        let target = directory.join(&file);
        if source.is_file() {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(source, target);
        }
    }
    Ok(())
}

pub fn remove(id: &str) {
    let _ = std::fs::remove_dir_all(storage().join(id));
}

/// Diff a captured snapshot against another snapshot or the working tree.
pub fn diff(directory: &Path, from: &str, to: Option<&str>) -> Result<Vec<Value>> {
    let (from_meta, _) = load(from)?;
    let from_ref = from_meta
        .get("patch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("snapshot has no git reference"))?;
    let mut args: Vec<String> = vec!["diff".into(), "--no-ext-diff".into(), "--unified=3".into(), from_ref.into()];
    if let Some(to) = to {
        let (to_meta, _) = load(to)?;
        let to_ref = to_meta
            .get("patch")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("snapshot has no git reference"))?;
        args.push(to_ref.into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git(directory, &refs).unwrap_or_default();
    let mut entries = crate::vcs::parse_patch(&output);
    if to.is_none() {
        if let Value::Array(items) = &mut entries {
            items.extend(crate::vcs::untracked_patches(directory));
        }
    }
    let mut values = entries.as_array().cloned().unwrap_or_default();
    values.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn restores_tracked_and_untracked_files() {
        let dir = std::env::temp_dir().join(format!("opencode-snapshot-test-{}", crate::util::id("repo")));
        std::fs::create_dir_all(&dir).unwrap();
        git_ok(&dir, &["init"]);
        git_ok(&dir, &["config", "user.email", "test@example.com"]);
        git_ok(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git_ok(&dir, &["add", "tracked.txt"]);
        git_ok(&dir, &["commit", "-m", "initial"]);
        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let snapshot = capture(&dir).expect("snapshot");
        std::fs::write(dir.join("tracked.txt"), "three\n").unwrap();
        std::fs::write(dir.join("later.txt"), "later\n").unwrap();
        restore(&dir, &snapshot.id).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("tracked.txt")).unwrap().replace("\r\n", "\n"), "two\n");
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap().replace("\r\n", "\n"), "new\n");
        assert!(!dir.join("later.txt").exists());
        remove(&snapshot.id);
        let _ = std::fs::remove_dir_all(dir);
    }
}
