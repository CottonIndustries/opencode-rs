//! Logical workspaces and the local environment driver.
//!
//! A workspace is a durable binding to a provider-owned resource. This port
//! ships the local driver; remote providers can implement the same driver
//! interface without changing session or tool code.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub provider: String,
    pub directory: String,
    pub binding: Value,
    pub created_at: i64,
    pub last_used_at: i64,
}

fn db() -> Option<&'static Mutex<Connection>> {
    static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
    if let Some(db) = DB.get() {
        return Some(db);
    }
    let path = crate::paths::get().storage().join("workspaces.db");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let connection = Connection::open(&path).ok()?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                directory TEXT NOT NULL,
                binding TEXT,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS workspaces_provider ON workspaces(provider, last_used_at);",
        )
        .ok()?;
    let _ = DB.set(Mutex::new(connection));
    DB.get()
}

pub trait Driver: Send + Sync {
    fn name(&self) -> &'static str;
    fn binding(&self, id: &str, directory: &Path) -> Result<Value>;
    fn environment(&self, info: &Info) -> Result<Environment>;
    fn destroy(&self, _info: &Info) -> Result<()> {
        Ok(())
    }
}

pub struct LocalDriver;

impl Driver for LocalDriver {
    fn name(&self) -> &'static str {
        "local"
    }

    fn binding(&self, _id: &str, directory: &Path) -> Result<Value> {
        Ok(json!({ "directory": directory.to_string_lossy() }))
    }

    fn environment(&self, info: &Info) -> Result<Environment> {
        let directory = info
            .binding
            .get("directory")
            .and_then(Value::as_str)
            .unwrap_or(&info.directory);
        Environment::new(directory)
    }
}

fn driver(provider: &str) -> Result<&'static dyn Driver> {
    static LOCAL: LocalDriver = LocalDriver;
    match provider {
        "local" | "worktree" => Ok(&LOCAL),
        other => Err(anyhow!("workspace provider not found: {other}")),
    }
}

fn row_to_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<Info> {
    let binding: Option<String> = row.get("binding")?;
    Ok(Info {
        id: row.get("id")?,
        provider: row.get("provider")?,
        directory: row.get("directory")?,
        binding: binding
            .and_then(|binding| serde_json::from_str(&binding).ok())
            .unwrap_or(Value::Null),
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
    })
}

pub fn create(id: Option<&str>, provider: &str, directory: &Path) -> Result<Info> {
    driver(provider)?;
    let id = id.unwrap_or("").to_string();
    let id = if id.is_empty() { crate::util::id("wrk") } else { id };
    let directory = std::fs::canonicalize(directory)
        .map(|path| crate::app::strip_unc(&path))
        .unwrap_or_else(|_| directory.to_path_buf());
    let binding = driver(provider)?.binding(&id, &directory)?;
    let now = crate::util::now_ms();
    if let Some(existing) = get(&id)? {
        if existing.provider != provider {
            return Err(anyhow!(
                "workspace {id} already exists with provider {}",
                existing.provider
            ));
        }
        return Ok(existing);
    }
    let db = db().ok_or_else(|| anyhow!("workspace database unavailable"))?;
    let db = db.lock().map_err(|_| anyhow!("workspace database unavailable"))?;
    db.execute(
        "INSERT INTO workspaces(id, provider, directory, binding, created_at, last_used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id, provider, directory.to_string_lossy(), binding.to_string(), now],
    )?;
    Ok(Info {
        id,
        provider: provider.to_string(),
        directory: directory.to_string_lossy().to_string(),
        binding,
        created_at: now,
        last_used_at: now,
    })
}

pub fn list() -> Vec<Info> {
    let Some(db) = db() else { return Vec::new() };
    let Ok(db) = db.lock() else { return Vec::new() };
    let Ok(mut statement) = db.prepare("SELECT * FROM workspaces ORDER BY last_used_at DESC") else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], row_to_info) else { return Vec::new() };
    rows.flatten().collect()
}

pub fn get(id: &str) -> Result<Option<Info>> {
    let Some(db) = db() else { return Ok(None) };
    let db = db.lock().map_err(|_| anyhow!("workspace database unavailable"))?;
    let mut statement = db.prepare("SELECT * FROM workspaces WHERE id = ?1")?;
    let mut rows = statement.query(params![id])?;
    Ok(rows.next()?.and_then(|row| row_to_info(row).ok()))
}

pub fn provision(id: &str) -> Result<Info> {
    let Some(info) = get(id)? else { return Err(anyhow!("workspace not found: {id}")) };
    if info.binding.is_null() {
        let binding = driver(&info.provider)?.binding(&info.id, Path::new(&info.directory))?;
        if let Some(db) = db() {
            if let Ok(db) = db.lock() {
                let _ = db.execute(
                    "UPDATE workspaces SET binding = ?2, last_used_at = ?3 WHERE id = ?1",
                    params![id, binding.to_string(), crate::util::now_ms()],
                );
            }
        }
        return get(id)?.ok_or_else(|| anyhow!("workspace not found: {id}"));
    }
    touch(id);
    Ok(info)
}

pub fn connect(id: &str) -> Result<Environment> {
    let info = provision(id)?;
    driver(&info.provider)?.environment(&info)
}

pub fn touch(id: &str) {
    if let Some(db) = db() {
        if let Ok(db) = db.lock() {
            let _ = db.execute(
                "UPDATE workspaces SET last_used_at = ?2 WHERE id = ?1",
                params![id, crate::util::now_ms()],
            );
        }
    }
}

pub fn destroy(id: &str) -> Result<bool> {
    let Some(info) = get(id)? else { return Ok(false) };
    driver(&info.provider)?.destroy(&info)?;
    if let Some(db) = db() {
        if let Ok(db) = db.lock() {
            let _ = db.execute("DELETE FROM workspaces WHERE id = ?1", params![id]);
        }
    }
    Ok(true)
}

/// A local environment rooted at a workspace directory.
#[derive(Debug, Clone)]
pub struct Environment {
    root: PathBuf,
}

impl Environment {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let root = std::fs::canonicalize(path.as_ref())
            .map(|path| crate::app::strip_unc(&path))
            .unwrap_or_else(|_| path.as_ref().to_path_buf());
        std::fs::create_dir_all(&root)?;
        Ok(Environment { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        if path.is_absolute() {
            return Err(anyhow!("workspace path must be relative: {}", path.display()));
        }
        if path.components().any(|component| matches!(component, Component::ParentDir)) {
            return Err(anyhow!("workspace path escapes the root: {}", path.display()));
        }
        Ok(self.root.join(path))
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.resolve(path)?)?)
    }

    pub fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let path = self.resolve(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }

    pub fn stat(&self, path: &str) -> Result<Value> {
        let path = self.resolve(path)?;
        let meta = std::fs::symlink_metadata(&path)?;
        let kind = if meta.is_file() {
            "file"
        } else if meta.is_dir() {
            "directory"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else {
            "other"
        };
        Ok(json!({
            "type": kind,
            "size": meta.len(),
            "mtime": meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|time| time.as_millis() as i64),
        }))
    }

    pub fn list(&self, path: &str) -> Result<Vec<Value>> {
        let path = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            out.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": if file_type.is_file() { "file" } else if file_type.is_dir() { "directory" } else if file_type.is_symlink() { "symlink" } else { "other" },
            }));
        }
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(out)
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        let path = self.resolve(path)?;
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn move_path(&self, from: &str, to: &str) -> Result<()> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = from.file_name().map(|name| name.to_os_string());
        if let Ok(meta) = std::fs::symlink_metadata(&from) {
            if meta.is_file() && to.is_dir() {
                return std::fs::rename(&from, to.join(name.unwrap_or_default())).map_err(Into::into);
            }
        }
        std::fs::rename(from, to)?;
        Ok(())
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        std::fs::create_dir_all(self.resolve(path)?)?;
        Ok(())
    }

    pub fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(crate::tools::shell::detect().program);
        command.current_dir(&self.root);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_workspaces_round_trip() {
        let root = std::env::temp_dir().join(format!("opencode-workspace-test-{}", crate::util::id("dir")));
        std::fs::create_dir_all(&root).unwrap();
        let info = create(None, "local", &root).unwrap();
        let environment = connect(&info.id).unwrap();
        environment.write("notes.txt", b"hello").unwrap();
        assert_eq!(environment.read("notes.txt").unwrap(), b"hello");
        assert!(environment.resolve("../escape").is_err());
        assert!(list().iter().any(|item| item.id == info.id));
        assert!(destroy(&info.id).unwrap());
        assert!(get(&info.id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
