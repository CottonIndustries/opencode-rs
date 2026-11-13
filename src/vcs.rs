//! Built-in git integration for the plugin VCS and worktree domains.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(if error.is_empty() { "git command failed".into() } else { error }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn optional(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok().map(|out| out.trim().to_string()).filter(|out| !out.is_empty())
}

fn scoped(input: &Value) -> PathBuf {
    input
        .get("worktree")
        .or_else(|| input.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn current_branch(dir: &Path) -> Option<String> {
    optional(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD")
}

fn default_branch(dir: &Path) -> Option<(String, String)> {
    if let Some(remote) = optional(dir, &["remote"]).and_then(|list| list.lines().next().map(String::from)) {
        if let Some(head) = optional(dir, &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")]) {
            let short = head.trim_start_matches("refs/remotes/");
            let name = short.strip_prefix(&format!("{remote}/")).unwrap_or(short);
            if !name.is_empty() {
                return Some((name.to_string(), short.to_string()));
            }
        }
    }
    for name in ["main", "master"] {
        if git(dir, &["rev-parse", "--verify", &format!("refs/heads/{name}")]).is_ok() {
            return Some((name.to_string(), name.to_string()));
        }
    }
    optional(dir, &["config", "init.defaultBranch"]).map(|name| (name.clone(), name))
}

fn has_head(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--verify", "HEAD"]).is_ok()
}

pub fn info(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    let branch = current_branch(&dir);
    let default = default_branch(&dir).map(|(name, _)| name);
    Ok(json!({
        "provider": "git",
        "branch": {
            "current": branch,
            "default": default,
        }
    }))
}

pub fn base(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    if !has_head(&dir) {
        return Ok(Value::Null);
    }
    let (name, reference) = default_branch(&dir).unwrap_or_else(|| ("main".into(), "main".into()));
    Ok(json!({ "name": name, "ref": reference, "source": "default" }))
}

pub fn branches(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    let search = input.get("search").and_then(Value::as_str).map(|s| s.to_lowercase());
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    let output = git(&dir, &["branch", "--format=%(refname:short)"])?;
    let mut names: Vec<Value> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| search.as_deref().is_none_or(|search| line.to_lowercase().contains(search)))
        .take(limit)
        .map(|line| json!(line))
        .collect();
    names.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    Ok(Value::Array(names))
}

pub fn init(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    git(&dir, &["init"])?;
    Ok(Value::Null)
}

fn numstat(dir: &Path, args: &[&str]) -> BTreeMap<String, (u64, u64)> {
    let mut map = BTreeMap::new();
    let Ok(output) = git(dir, args) else { return map };
    for line in output.lines() {
        let mut fields = line.splitn(3, '\t');
        let Some(add) = fields.next() else { continue };
        let Some(del) = fields.next() else { continue };
        let Some(file) = fields.next() else { continue };
        let count = |value: &str| value.parse::<u64>().unwrap_or(0);
        map.insert(file.to_string(), (count(add), count(del)));
    }
    map
}

fn untracked_size(dir: &Path, file: &str) -> (u64, u64) {
    std::fs::read_to_string(dir.join(file))
        .map(|text| (text.lines().count().max(1) as u64, 0))
        .unwrap_or((0, 0))
}

pub fn status(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    let output = git(&dir, &["status", "--porcelain=v1", "-z"])?;
    let stats = numstat(&dir, &["diff", "--numstat", "HEAD"]);
    let mut files: BTreeMap<String, (String, u64, u64)> = BTreeMap::new();
    let mut items = output.split('\0');
    while let Some(item) = items.next() {
        if item.len() < 3 {
            continue;
        }
        let code = &item[..2];
        let file = item[3..].to_string();
        // Renames put the original path in the next NUL-separated field.
        if code.starts_with('R') || code.starts_with('C') {
            let _ = items.next();
        }
        let status = if code == "??" || code.contains('A') {
            "added"
        } else if code.contains('D') {
            "deleted"
        } else {
            "modified"
        };
        let (additions, deletions) = stats
            .get(&file)
            .copied()
            .or_else(|| (status == "added").then(|| untracked_size(&dir, &file)))
            .unwrap_or((0, 0));
        files.insert(file, (status.to_string(), additions, deletions));
    }
    Ok(Value::Array(
        files
            .into_iter()
            .map(|(file, (status, additions, deletions))| {
                json!({ "file": file, "additions": additions, "deletions": deletions, "status": status })
            })
            .collect(),
    ))
}

fn split_patch(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            if let Some((file, patch)) = current.take() {
                out.push((file, patch));
            }
            let file = line
                .split(" b/")
                .nth(1)
                .or_else(|| line.split(" a/").nth(1))
                .unwrap_or("")
                .to_string();
            current = Some((file, format!("{line}\n")));
        } else if let Some((_, patch)) = current.as_mut() {
            patch.push_str(line);
            patch.push('\n');
        }
    }
    if let Some((file, patch)) = current {
        out.push((file, patch));
    }
    out
}

pub(crate) fn parse_patch(input: &str) -> Value {
    let mut entries: Vec<Value> = split_patch(input)
        .into_iter()
        .filter(|(file, _)| !file.is_empty())
        .map(|(file, patch)| {
            let additions = patch
                .lines()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                .count() as u64;
            let deletions = patch
                .lines()
                .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                .count() as u64;
            let status = if patch.contains("\nnew file mode") {
                "added"
            } else if patch.contains("\ndeleted file mode") {
                "deleted"
            } else {
                "modified"
            };
            json!({
                "file": file,
                "patch": patch,
                "additions": additions,
                "deletions": deletions,
                "status": status,
            })
        })
        .collect();
    entries.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    Value::Array(entries)
}

pub(crate) fn untracked_patches(dir: &Path) -> Vec<Value> {
    let Ok(output) = git(dir, &["ls-files", "--others", "--exclude-standard", "-z"]) else {
        return Vec::new();
    };
    output
        .split('\0')
        .filter(|file| !file.is_empty())
        .filter_map(|file| {
            let text = std::fs::read_to_string(dir.join(file)).ok()?;
            let mut patch = format!("diff --git a/{file} b/{file}\nnew file mode 100644\n--- /dev/null\n+++ b/{file}\n");
            for line in text.lines() {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
            Some(json!({
                "file": file,
                "patch": patch,
                "additions": text.lines().count() as u64,
                "deletions": 0,
                "status": "added",
            }))
        })
        .collect()
}

pub fn diff(input: &Value) -> Result<Value> {
    let dir = scoped(input);
    let mode = input.get("mode").and_then(Value::as_str).unwrap_or("working");
    let context = input.get("context").and_then(Value::as_u64).unwrap_or(3);
    let base = input.get("base").and_then(Value::as_str);
    let unified = format!("--unified={context}");
    let mut args: Vec<String> = vec!["diff".into(), "--no-ext-diff".into(), unified];
    match mode {
        "working" => {
            if has_head(&dir) {
                args.push("HEAD".into());
            }
        }
        _ => {
            let base = base
                .map(String::from)
                .or_else(|| default_branch(&dir).map(|(_, reference)| reference))
                .ok_or_else(|| anyhow!("No review base available"))?;
            args.push(format!("{base}...HEAD"));
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git(&dir, &refs)?;
    let mut entries = parse_patch(&output);
    if mode == "working" {
        if let Value::Array(items) = &mut entries {
            items.extend(untracked_patches(&dir));
        }
    }
    Ok(entries)
}

pub fn dispatch(method: &str, input: &Value) -> Result<Value> {
    match method {
        "vcs.init" => init(input),
        "vcs.info" => info(input),
        "vcs.base" => base(input),
        "vcs.branches" => branches(input),
        "vcs.status" => status(input),
        "vcs.diff" => diff(input),
        "worktree.list" => worktree_list(input),
        "worktree.create" => worktree_create(input),
        "worktree.remove" => worktree_remove(input),
        "worktree.refresh" => worktree_refresh(input),
        other => Err(anyhow!("unsupported vcs method: {other}")),
    }
}

fn worktree_list(input: &Value) -> Result<Value> {
    let source = input
        .get("sourceDirectory")
        .or_else(|| input.get("worktree"))
        .or_else(|| input.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = optional(&source, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| source.clone());
    let output = git(&root, &["worktree", "list", "--porcelain"])?;
    let mut entries: Vec<Value> = Vec::new();
    for line in output.lines() {
        let Some(path) = line.strip_prefix("worktree ") else { continue };
        let path = PathBuf::from(path);
        let kind = if same_path(&path, &root) { "root" } else { "worktree" };
        entries.push(json!({ "directory": path.to_string_lossy(), "type": kind }));
    }
    Ok(Value::Array(entries))
}

fn same_path(left: &Path, right: &Path) -> bool {
    crate::util::normalize(left) == crate::util::normalize(right)
}

fn worktree_refresh(input: &Value) -> Result<Value> {
    let source = input
        .get("sourceDirectory")
        .or_else(|| input.get("worktree"))
        .or_else(|| input.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = optional(&source, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("not a git repository"))?;
    git(&root, &["worktree", "prune"])?;
    worktree_list(input)
}

fn worktree_create(input: &Value) -> Result<Value> {
    let source = input
        .get("sourceDirectory")
        .or_else(|| input.get("from"))
        .or_else(|| input.get("worktree"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = optional(&source, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| source.clone());
    let directory = input
        .get("directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            let name = input.get("name").and_then(Value::as_str)?;
            Some(root.parent()?.join(format!("{}-{name}", root.file_name()?.to_string_lossy())))
        })
        .ok_or_else(|| anyhow!("worktree.create requires directory or name"))?;
    let mut args = vec!["worktree", "add"];
    let branch = input.get("branch").and_then(Value::as_str);
    args.push(directory.to_str().unwrap_or_default());
    if let Some(branch) = branch {
        args.push(branch);
    }
    git(&root, &args)?;
    Ok(json!({ "directory": directory.to_string_lossy() }))
}

fn worktree_remove(input: &Value) -> Result<Value> {
    let source = input
        .get("sourceDirectory")
        .or_else(|| input.get("worktree"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = optional(&source, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or(source);
    let directory = input
        .get("directory")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("worktree.remove requires directory"))?;
    let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(directory);
    git(&root, &args)?;
    Ok(Value::Null)
}

#[allow(dead_code)]
fn string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

#[allow(dead_code)]
fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("opencode-vcs-test-{}", crate::util::id("repo")));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init"]).unwrap();
        git(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        git(&dir, &["config", "user.name", "Test"]).unwrap();
        std::fs::write(dir.join("file.txt"), "one\n").unwrap();
        git(&dir, &["add", "file.txt"]).unwrap();
        git(&dir, &["commit", "-m", "initial"]).unwrap();
        dir
    }

    #[test]
    fn status_and_diff_report_working_changes() {
        let dir = repo();
        std::fs::write(dir.join("file.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let input = json!({ "worktree": dir.to_string_lossy() });
        let status = status(&input).unwrap();
        assert!(status.as_array().unwrap().iter().any(|item| item["file"] == "file.txt" && item["status"] == "modified"));
        assert!(status.as_array().unwrap().iter().any(|item| item["file"] == "new.txt" && item["status"] == "added"));
        let diff = diff(&json!({
            "worktree": dir.to_string_lossy(),
            "mode": "working",
            "context": 3
        }))
        .unwrap();
        assert!(diff.as_array().unwrap().iter().any(|item| item["file"] == "file.txt" && item["additions"] == 1));
        assert!(diff.as_array().unwrap().iter().any(|item| item["file"] == "new.txt" && item["status"] == "added"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn info_and_branches_use_git() {
        let dir = repo();
        let input = json!({ "worktree": dir.to_string_lossy() });
        let info = info(&input).unwrap();
        assert!(info["branch"]["current"].as_str().is_some_and(|branch| !branch.is_empty()));
        let branches = branches(&input).unwrap();
        assert!(branches.as_array().unwrap().iter().any(|branch| branch.as_str().is_some()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
