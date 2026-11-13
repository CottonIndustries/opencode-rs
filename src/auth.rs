//! Credential storage in `<data>/auth.json`, compatible with the legacy format:
//! `{ "<provider>": { "type": "api", "key": "..." } }`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Credential {
    Api {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, String>>,
    },
    Oauth {
        refresh: String,
        access: String,
        expires: i64,
        #[serde(default, rename = "accountId", skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, rename = "enterpriseUrl", skip_serializing_if = "Option::is_none")]
        enterprise_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    Wellknown {
        key: String,
        token: String,
    },
}

impl Credential {
    pub fn secret(&self) -> &str {
        match self {
            Credential::Api { key, .. } => key,
            Credential::Oauth { access, .. } => access,
            Credential::Wellknown { token, .. } => token,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Credential::Api { .. } => "api",
            Credential::Oauth { .. } => "oauth",
            Credential::Wellknown { .. } => "wellknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential: Credential,
    pub active: bool,
}

fn credential_from_value(value: &serde_json::Value) -> Option<Credential> {
    if value.get("type").and_then(|value| value.as_str()) == Some("key") {
        return value
            .get("key")
            .and_then(|value| value.as_str())
            .map(|key| Credential::Api { key: key.to_string(), metadata: None });
    }
    serde_json::from_value(value.clone()).ok()
}

/// Every credential known to this installation, including inactive database
/// accounts used by auth switch.
pub fn entries() -> Vec<Entry> {
    let mut out = Vec::new();
    let path = crate::paths::get().data.join("opencode.db");
    if path.is_file() {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if let Ok(db) = rusqlite::Connection::open_with_flags(&path, flags) {
            let _ = db.busy_timeout(std::time::Duration::from_millis(500));
            let statement = db.prepare(
                "SELECT id, integration_id, label, value, coalesce(active, 0) FROM credential ORDER BY time_updated",
            );
            if let Ok(mut statement) = statement {
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                });
                for (id, provider, label, value, active) in rows.into_iter().flatten().flatten() {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&value) else { continue };
                    if let Some(credential) = credential_from_value(&value) {
                        out.push(Entry {
                            id,
                            provider: provider.trim_end_matches('/').to_string(),
                            label,
                            credential,
                            active: active != 0,
                        });
                    }
                }
            }
        }
    }
    for (provider, credential) in from_file() {
        if out.iter().any(|entry| entry.provider == provider && entry.credential.secret() == credential.secret()) {
            continue;
        }
        out.push(Entry {
            id: format!("file:{provider}"),
            provider: provider.clone(),
            label: provider,
            credential,
            active: true,
        });
    }
    out
}

pub fn switch(provider: &str, target: &str) -> Result<Entry> {
    let entries = entries();
    let entry = entries
        .iter()
        .find(|entry| entry.provider == provider && (entry.id == target || entry.label == target))
        .or_else(|| entries.iter().find(|entry| entry.id == target || entry.label == target))
        .ok_or_else(|| anyhow::anyhow!("credential not found: {target}"))?
        .clone();
    if let Ok(db) = rusqlite::Connection::open(crate::paths::get().data.join("opencode.db")) {
        let _ = db.execute("UPDATE credential SET active = 0 WHERE integration_id = ?1", [&entry.provider]);
        let _ = db.execute("UPDATE credential SET active = 1 WHERE id = ?1", [&entry.id]);
    }
    set(&entry.provider, entry.credential.clone())?;
    Ok(entry)
}

/// Merge exported credentials into the local store. Existing providers are
/// skipped so an import never replaces the active account.
pub fn import(values: &[serde_json::Value]) -> Result<(usize, usize)> {
    let mut map = from_file();
    let mut imported = 0;
    let mut skipped = 0;
    for value in values {
        let provider = value
            .get("provider")
            .or_else(|| value.get("integrationID"))
            .or_else(|| value.get("integration_id"))
            .and_then(|value| value.as_str());
        let Some(provider) = provider else {
            skipped += 1;
            continue;
        };
        let provider = provider.trim_end_matches('/');
        if map.contains_key(provider) {
            skipped += 1;
            continue;
        }
        let raw = value
            .get("credential")
            .or_else(|| value.get("value"))
            .unwrap_or(value);
        let Some(credential) = credential_from_value(raw) else {
            skipped += 1;
            continue;
        };
        map.insert(provider.to_string(), credential);
        imported += 1;
    }
    write(&map)?;
    Ok((imported, skipped))
}

/// Stored credentials: auth.json first, then API keys saved by the TypeScript
/// client in its `opencode.db` (read-only; OAuth entries there need refresh flows and are skipped).
pub fn all() -> BTreeMap<String, Credential> {
    let mut out = from_database(&crate::paths::get().data.join("opencode.db"));
    out.extend(from_file());
    out
}

fn from_file() -> BTreeMap<String, Credential> {
    let Ok(text) = std::fs::read_to_string(crate::paths::get().auth_file()) else {
        return BTreeMap::new();
    };
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    raw.into_iter().filter_map(|(k, v)| serde_json::from_value(v).ok().map(|c| (k.trim_end_matches('/').to_string(), c))).collect()
}

pub fn from_database(path: &std::path::Path) -> BTreeMap<String, Credential> {
    let mut out = BTreeMap::new();
    if !path.is_file() {
        return out;
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(db) = rusqlite::Connection::open_with_flags(path, flags) else { return out };
    let _ = db.busy_timeout(std::time::Duration::from_millis(500));
    // prefer active credentials; older schemas have no `active` column
    let stmt = db
        .prepare("SELECT integration_id, value FROM credential ORDER BY coalesce(active, 0), time_updated")
        .or_else(|_| db.prepare("SELECT integration_id, value FROM credential ORDER BY time_updated"));
    let Ok(mut stmt) = stmt else { return out };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    for (id, value) in rows.into_iter().flatten().flatten() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&value) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("key") {
            continue;
        }
        if let Some(key) = v.get("key").and_then(|k| k.as_str()).filter(|k| !k.is_empty()) {
            out.insert(id.trim_end_matches('/').to_string(), Credential::Api { key: key.to_string(), metadata: None });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_api_keys_from_database() {
        let path = std::env::temp_dir().join(format!("opencode-auth-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL, value text NOT NULL, connector_id text, method_id text, active integer, time_created integer NOT NULL, time_updated integer NOT NULL);
                 INSERT INTO credential VALUES ('1', 'anthropic', 'API key', '{\"type\":\"key\",\"key\":\"sk-active\"}', NULL, NULL, 1, 1, 1);
                 INSERT INTO credential VALUES ('3', 'anthropic', 'old key', '{\"type\":\"key\",\"key\":\"sk-inactive\"}', NULL, NULL, 0, 1, 5);
                 INSERT INTO credential VALUES ('2', 'openai', 'OAuth', '{\"type\":\"oauth\",\"access\":\"a\",\"refresh\":\"r\",\"expires\":0}', NULL, NULL, 1, 1, 1);",
            )
            .unwrap();
        }
        let creds = super::from_database(&path);
        assert_eq!(creds.get("anthropic").map(|c| c.secret()), Some("sk-active"));
        assert!(!creds.contains_key("openai"));
        let _ = std::fs::remove_file(&path);
    }
}

pub fn get(provider: &str) -> Option<Credential> {
    all().remove(provider)
}

fn write(map: &BTreeMap<String, Credential>) -> Result<()> {
    let path = crate::paths::get().auth_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(map)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn set(provider: &str, cred: Credential) -> Result<()> {
    let mut map = from_file();
    map.insert(provider.to_string(), cred);
    write(&map)
}

pub fn remove(provider: &str) -> Result<bool> {
    let mut map = from_file();
    let existed = map.remove(provider).is_some();
    write(&map)?;
    Ok(existed)
}
