//! Local tracking checkouts for remote git references.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    crate::paths::get().storage().join("repos")
}

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(anyhow!("git {} failed", args.first().copied().unwrap_or("")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-'))
}

fn cache_path(repository: &str, branch: Option<&str>) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(repository.as_bytes());
    hasher.update(b"\0");
    hasher.update(branch.unwrap_or_default().as_bytes());
    root().join(format!("{:x}", hasher.finalize()))
}

/// Clone or refresh a remote reference into its local cache.
pub fn ensure(repository: &str, branch: Option<&str>, refresh: bool) -> Result<Value> {
    if repository.trim().is_empty() {
        return Err(anyhow!("repository is required"));
    }
    if let Some(branch) = branch {
        if !valid_branch(branch) {
            return Err(anyhow!("invalid branch: {branch}"));
        }
    }
    let path = cache_path(repository, branch);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = path.join(".git").exists();
    let status = if !existing {
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        let mut command = Command::new("git");
        command.arg("clone").arg("--depth").arg("1");
        if let Some(branch) = branch {
            command.arg("--branch").arg(branch);
        }
        command.arg("--").arg(repository).arg(&path);
        command.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        "cloned"
    } else if refresh {
        let _ = git(&path, &["fetch", "--all", "--prune"])?;
        let target = match branch {
            Some(branch) => format!("origin/{branch}"),
            None => "origin/HEAD".to_string(),
        };
        let _ = git(&path, &["reset", "--hard", &target])?;
        "refreshed"
    } else {
        "cached"
    };
    let head = git(&path, &["rev-parse", "HEAD"]).unwrap_or_default();
    let current = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    Ok(json!({
        "repository": repository,
        "localPath": path.to_string_lossy(),
        "status": status,
        "head": head,
        "branch": branch.unwrap_or(&current),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_and_refreshes_local_references() {
        let source = std::env::temp_dir().join(format!("opencode-repository-source-{}", crate::util::id("repo")));
        std::fs::create_dir_all(&source).unwrap();
        let git_ok = |args: &[&str]| {
            let mut command = Command::new("git");
            command.args(args).current_dir(&source).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            assert!(command.status().unwrap().success());
        };
        git_ok(&["init"]);
        git_ok(&["config", "user.email", "test@example.com"]);
        git_ok(&["config", "user.name", "Test"]);
        std::fs::write(source.join("README.md"), "one\n").unwrap();
        git_ok(&["add", "README.md"]);
        git_ok(&["commit", "-m", "initial"]);
        let remote = source.to_string_lossy().to_string();
        let cached = ensure(&remote, None, false).unwrap();
        assert_eq!(cached["status"], "cloned");
        let path = PathBuf::from(cached["localPath"].as_str().unwrap());
        assert!(path.join("README.md").is_file());
        assert_eq!(ensure(&remote, None, false).unwrap()["status"], "cached");
        let _ = std::fs::remove_dir_all(path);
        let _ = std::fs::remove_dir_all(source);
    }
}
