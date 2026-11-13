//! Content-addressed media blobs produced by image generation.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

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
    Ok(hash)
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
