//! Content-addressed media blobs produced by image generation.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn dir() -> PathBuf {
    crate::paths::get().storage().join("media")
}

fn blob(hash: &str) -> PathBuf {
    dir().join(hash)
}

fn mime_file(hash: &str) -> PathBuf {
    dir().join(format!("{hash}.mime"))
}

pub fn put(data: &[u8], mime: &str) -> Result<String> {
    let hash = format!("{:x}", Sha256::digest(data));
    std::fs::create_dir_all(dir())?;
    if !blob(&hash).is_file() {
        std::fs::write(blob(&hash), data)?;
        std::fs::write(mime_file(&hash), mime)?;
    }
    recent().lock().unwrap().insert(hash.clone(), crate::util::now_ms() + 300_000);
    Ok(hash)
}

fn recent() -> &'static Mutex<HashMap<String, i64>> {
    static RECENT: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    RECENT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_recent(hash: &str) -> bool {
    let now = crate::util::now_ms();
    let mut recent = recent().lock().unwrap();
    recent.retain(|_, expires| *expires > now);
    recent.contains_key(hash)
}

fn in_json_dir(dir: &std::path::Path, hash: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if in_json_dir(&path, hash) {
                return true;
            }
        } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
            if std::fs::read_to_string(path).is_ok_and(|text| text.contains(hash)) {
                return true;
            }
        }
    }
    false
}

pub fn referenced(hash: &str) -> bool {
    is_recent(hash)
        || crate::db::contains(hash)
        || crate::db::message_contains(hash)
        || crate::event_store::contains(hash)
        || in_json_dir(&crate::paths::get().storage().join("session"), hash)
}

pub fn get(hash: &str) -> Option<(Vec<u8>, String)> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let data = std::fs::read(blob(hash)).ok()?;
    let mime = std::fs::read_to_string(mime_file(hash)).unwrap_or_else(|_| "application/octet-stream".into());
    Some((data, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_addressed_round_trip() {
        let hash = put(b"media test", "text/plain").unwrap();
        let (data, mime) = get(&hash).unwrap();
        assert_eq!(data, b"media test");
        assert_eq!(mime, "text/plain");
        assert!(get("../nope").is_none());
    }
}

pub fn url(hash: &str) -> String {
    format!("/api/experimental/media/{hash}")
}
