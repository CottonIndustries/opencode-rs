//! Local session sharing.
//!
//! A share is a static transcript bundle under `storage/shares/<id>` served by
//! the HTTP server at `/share/<id>`. It does not upload content anywhere.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

fn root() -> PathBuf {
    crate::paths::get().storage().join("shares")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn share(session: &crate::session::Session) -> Result<Value> {
    let id = crate::util::id("shr");
    let directory = root().join(&id);
    std::fs::create_dir_all(&directory)?;
    let transcript = serde_json::to_vec_pretty(session)?;
    std::fs::write(directory.join("session.json"), transcript)?;
    let mut body = String::new();
    body.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>");
    body.push_str(&escape(&session.title));
    body.push_str("</title><style>body{font:16px system-ui;max-width:900px;margin:40px auto;padding:0 20px;color:#222}pre{white-space:pre-wrap;background:#f5f5f5;padding:12px;border-radius:6px}.m{margin:18px 0}.role{font-weight:600;color:#555}</style></head><body><h1>");
    body.push_str(&escape(&session.title));
    body.push_str("</h1>");
    for message in &session.messages {
        body.push_str("<div class=\"m\"><div class=\"role\">");
        body.push_str(match message.role {
            crate::session::Role::User => "User",
            crate::session::Role::Assistant => "Assistant",
        });
        body.push_str("</div><pre>");
        body.push_str(&escape(&message.text()));
        body.push_str("</pre></div>");
    }
    body.push_str("</body></html>");
    std::fs::write(directory.join("index.html"), body)?;
    Ok(json!({
        "id": id,
        "url": format!("/share/{id}"),
        "path": directory.to_string_lossy(),
    }))
}

pub fn revoke(id: &str) {
    let _ = std::fs::remove_dir_all(root().join(id));
}

pub fn html(id: &str) -> Option<String> {
    if id.is_empty() || !id.chars().all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-') {
        return None;
    }
    std::fs::read_to_string(root().join(id).join("index.html")).ok()
}

pub fn data(id: &str) -> Option<String> {
    if id.is_empty() || !id.chars().all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-') {
        return None;
    }
    std::fs::read_to_string(root().join(id).join("session.json")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_render_and_revoke() {
        let mut session = crate::session::Session::new("project", &std::env::temp_dir(), None);
        session.messages.push(crate::session::Message::user_text("hello <world>"));
        let share = share(&session).unwrap();
        let id = share["id"].as_str().unwrap();
        let page = html(id).unwrap();
        assert!(page.contains("hello &lt;world&gt;"));
        assert!(data(id).unwrap().contains("hello <world>"));
        revoke(id);
        assert!(html(id).is_none());
    }
}
