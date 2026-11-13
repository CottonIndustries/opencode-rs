//! Content-addressed working-tree snapshots used by session revert.
//!
//! Every captured file is stored by SHA-256 under `storage/snapshots/objects`.
//! A tree manifest at `storage/snapshots/trees/<id>.json` records the path,
//! hash and mode of each file. Older patch-based snapshots are still readable
//! through the legacy fallback so existing sessions keep reverting.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet};
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

fn objects() -> PathBuf {
    storage().join("objects")
}

fn trees() -> PathBuf {
    storage().join("trees")
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Entry {
    hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Tree {
    version: u32,
    files: BTreeMap<String, Entry>,
}

pub struct Snapshot {
    pub id: String,
}

fn files(directory: &Path) -> Vec<String> {
    git(directory, &["ls-files", "-co", "--exclude-standard", "-z"])
        .map(|list| {
            list.split('\0')
                .filter(|file| !file.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).ok().map(|meta| meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn set_mode(path: &Path, mode: Option<u32>) {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn safe_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute() && !path.components().any(|component| matches!(component, std::path::Component::ParentDir))
}

/// Capture the current working tree as a content-addressed tree. Returns
/// `None` outside a git repository.
pub fn capture(directory: &Path) -> Option<Snapshot> {
    if !is_repo(directory) {
        return None;
    }
    std::fs::create_dir_all(objects()).ok()?;
    std::fs::create_dir_all(trees()).ok()?;
    let mut tree = Tree { version: 1, files: BTreeMap::new() };
    for file in files(directory) {
        if !safe_relative(&file) {
            continue;
        }
        let path = directory.join(&file);
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let hash = hash(&bytes);
        let object = objects().join(&hash);
        if !object.is_file() {
            std::fs::write(&object, &bytes).ok()?;
        }
        tree.files.insert(file, Entry { hash, mode: mode_of(&path) });
    }
    let serialized = serde_json::to_vec(&tree.files).ok()?;
    let id = format!("snap_{}", hash(&serialized));
    let target = trees().join(format!("{id}.json"));
    if !target.is_file() {
        std::fs::write(&target, serde_json::to_vec(&tree).ok()?).ok()?;
    }
    Some(Snapshot { id })
}

fn load_tree(id: &str) -> Option<Tree> {
    let bytes = std::fs::read(trees().join(format!("{id}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_object(hash: &str) -> Result<Vec<u8>> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid snapshot object hash"));
    }
    Ok(std::fs::read(objects().join(hash))?)
}

fn restore_tree(directory: &Path, tree: &Tree) -> Result<()> {
    let current = files(directory);
    for file in current {
        if !tree.files.contains_key(&file) && safe_relative(&file) {
            let _ = std::fs::remove_file(directory.join(&file));
        }
    }
    for (file, entry) in &tree.files {
        if !safe_relative(file) {
            continue;
        }
        let target = directory.join(file);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, read_object(&entry.hash)?)?;
        set_mode(&target, entry.mode);
    }
    Ok(())
}

/// Restore the whole captured tree.
pub fn restore(directory: &Path, id: &str) -> Result<()> {
    if !is_repo(directory) {
        return Err(anyhow!("cannot restore a snapshot outside a git repository"));
    }
    if let Some(tree) = load_tree(id) {
        return restore_tree(directory, &tree);
    }
    legacy_restore(directory, id)
}

/// Restore only the selected project paths from a captured tree. A selected
/// path absent from the tree is removed; other paths stay untouched.
pub fn restore_files(directory: &Path, id: &str, selected: &[String]) -> Result<()> {
    let tree = load_tree(id).ok_or_else(|| anyhow!("snapshot not found: {id}"))?;
    for file in selected {
        if !safe_relative(file) {
            return Err(anyhow!("snapshot path escapes the project: {file}"));
        }
        let target = directory.join(file);
        match tree.files.get(file) {
            Some(entry) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, read_object(&entry.hash)?)?;
                set_mode(&target, entry.mode);
            }
            None => {
                let _ = std::fs::remove_file(target);
            }
        }
    }
    Ok(())
}

pub fn remove(id: &str) {
    let _ = std::fs::remove_file(trees().join(format!("{id}.json")));
    let _ = std::fs::remove_dir_all(storage().join(id));
}

/// Paths changed between two captured trees.
pub fn changed_files(directory: &Path, from: &str, to: Option<&str>) -> Result<Vec<String>> {
    let to_id = match to {
        Some(to) => to.to_string(),
        None => capture(directory)
            .map(|snapshot| snapshot.id)
            .ok_or_else(|| anyhow!("cannot capture the working tree"))?,
    };
    let from_tree = load_tree(from).ok_or_else(|| anyhow!("snapshot not found: {from}"))?;
    let to_tree = load_tree(&to_id).ok_or_else(|| anyhow!("snapshot not found: {to_id}"))?;
    let names: BTreeSet<&String> = from_tree.files.keys().chain(to_tree.files.keys()).collect();
    Ok(names
        .into_iter()
        .filter(|file| from_tree.files.get(*file).map(|entry| &entry.hash) != to_tree.files.get(*file).map(|entry| &entry.hash))
        .cloned()
        .collect())
}

/// Diff a captured tree against another tree or the working tree.
pub fn diff(directory: &Path, from: &str, to: Option<&str>) -> Result<Vec<Value>> {
    let Some(from_tree) = load_tree(from) else {
        return legacy_diff(directory, from, to);
    };
    let to_id = match to {
        Some(to) => to.to_string(),
        None => capture(directory)
            .map(|snapshot| snapshot.id)
            .ok_or_else(|| anyhow!("cannot capture the working tree"))?,
    };
    let Some(to_tree) = load_tree(&to_id) else {
        return legacy_diff(directory, from, to);
    };
    let names: BTreeSet<&String> = from_tree.files.keys().chain(to_tree.files.keys()).collect();
    let mut out = Vec::new();
    for file in names {
        let before = from_tree.files.get(file);
        let after = to_tree.files.get(file);
        if before.map(|entry| &entry.hash) == after.map(|entry| &entry.hash) {
            continue;
        }
        let old = before.map(|entry| read_object(&entry.hash)).transpose()?.unwrap_or_default();
        let new = after.map(|entry| read_object(&entry.hash)).transpose()?.unwrap_or_default();
        let patch = if old.contains(&0) || new.contains(&0) {
            format!("Binary files a/{file} and b/{file} differ\n")
        } else {
            let old = String::from_utf8_lossy(&old);
            let new = String::from_utf8_lossy(&new);
            let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
            let mut unified = diff.unified_diff();
            unified.context_radius(3).header(&format!("a/{file}"), &format!("b/{file}"));
            unified.to_string()
        };
        let additions = patch
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count() as u64;
        let deletions = patch
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count() as u64;
        let status = match (before, after) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            _ => "modified",
        };
        out.push(json!({
            "file": file,
            "patch": patch,
            "additions": additions,
            "deletions": deletions,
            "status": status,
        }));
    }
    out.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    Ok(out)
}

fn legacy_restore(directory: &Path, id: &str) -> Result<()> {
    let root = storage().join(id);
    let meta: Value = serde_json::from_slice(&std::fs::read(root.join("meta.json"))?)?;
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
            return Err(anyhow!("git checkout failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    let previous: Vec<String> = meta
        .get("untracked")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect())
        .unwrap_or_default();
    for file in files(directory) {
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

fn legacy_diff(directory: &Path, from: &str, to: Option<&str>) -> Result<Vec<Value>> {
    let (from_meta, _) = legacy_load(from)?;
    let from_ref = from_meta
        .get("patch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("snapshot has no git reference"))?;
    let mut args: Vec<String> = vec!["diff".into(), "--no-ext-diff".into(), "--unified=3".into(), from_ref.into()];
    if let Some(to) = to {
        let (to_meta, _) = legacy_load(to)?;
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

fn legacy_load(id: &str) -> Result<(Value, PathBuf)> {
    let root = storage().join(id);
    let meta = std::fs::read(root.join("meta.json"))?;
    let meta: Value = serde_json::from_slice(&meta)?;
    Ok((meta, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn captures_content_addressed_trees_and_restores_files() {
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
        let diffs = diff(&dir, &snapshot.id, None).unwrap();
        assert!(diffs.is_empty());
        remove(&snapshot.id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn diff_reports_added_and_modified_files() {
        let dir = std::env::temp_dir().join(format!("opencode-snapshot-diff-{}", crate::util::id("repo")));
        std::fs::create_dir_all(&dir).unwrap();
        git_ok(&dir, &["init"]);
        git_ok(&dir, &["config", "user.email", "test@example.com"]);
        git_ok(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git_ok(&dir, &["add", "a.txt"]);
        git_ok(&dir, &["commit", "-m", "initial"]);
        let first = capture(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let second = capture(&dir).unwrap();
        let diffs = diff(&dir, &first.id, Some(&second.id)).unwrap();
        assert!(diffs.iter().any(|item| item["file"] == "a.txt" && item["status"] == "modified"));
        assert!(diffs.iter().any(|item| item["file"] == "b.txt" && item["status"] == "added"));
        remove(&first.id);
        remove(&second.id);
        let _ = std::fs::remove_dir_all(dir);
    }
}
