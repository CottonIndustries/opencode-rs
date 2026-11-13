//! Per-session WebSocket transport state, matching the upstream
//! `SessionModelTransport`: one reused socket per session, affinity based on
//! URL plus sorted headers, 55 minute rotation, 15 second connect timeout,
//! 30 minute idle timeout, and a permanent HTTP fallback after repeated losses.

use anyhow::{anyhow, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use wreq::ws::message::Message;

pub const ROTATE_AFTER: Duration = Duration::from_secs(55 * 60);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const MAX_STREAM_FAILURES: u32 = 5;

struct Inner {
    closed: bool,
    http_fallback: bool,
    stream_failures: u32,
    channel: Option<Arc<Channel>>,
}

pub struct State {
    lock: Arc<AsyncMutex<()>>,
    inner: Mutex<Inner>,
}

pub struct Channel {
    socket: AsyncMutex<Option<wreq::ws::WebSocket>>,
    affinity: String,
    opened_at: Instant,
    checkpoint: Mutex<Option<Value>>,
}

static STATES: OnceLock<AsyncMutex<HashMap<String, Arc<State>>>> = OnceLock::new();

fn states() -> &'static AsyncMutex<HashMap<String, Arc<State>>> {
    STATES.get_or_init(|| AsyncMutex::new(HashMap::new()))
}

async fn state(session_id: &str) -> Arc<State> {
    let mut states = states().lock().await;
    states
        .entry(session_id.to_string())
        .or_insert_with(|| {
            Arc::new(State {
                lock: Arc::new(AsyncMutex::new(())),
                inner: Mutex::new(Inner {
                    closed: false,
                    http_fallback: false,
                    stream_failures: 0,
                    channel: None,
                }),
            })
        })
        .clone()
}

fn affinity(url: &str, headers: &[(String, String)]) -> String {
    let mut sorted = headers
        .iter()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("b3") && !key.eq_ignore_ascii_case("traceparent"))
        .cloned()
        .collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.update(b":");
    for (key, value) in sorted {
        hasher.update(key.as_bytes());
        hasher.update(b":");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    format!("{url}:{:x}", hasher.finalize())
}

/// One exclusive exchange on a session's transport.
pub struct Lease {
    state: Arc<State>,
    _guard: OwnedMutexGuard<()>,
    channel: Option<Arc<Channel>>,
}

impl Lease {
    pub async fn acquire(session_id: &str) -> Self {
        let state = state(session_id).await;
        let guard = state.lock.clone().lock_owned().await;
        let channel = state.inner.lock().unwrap().channel.clone();
        Lease { state, _guard: guard, channel }
    }

    pub fn http_fallback(&self) -> bool {
        self.state.inner.lock().unwrap().http_fallback
    }

    pub fn checkpoint(&self) -> Option<Value> {
        self.channel
            .as_ref()
            .and_then(|channel| channel.checkpoint.lock().unwrap().clone())
    }

    pub fn set_checkpoint(&self, checkpoint: Value) {
        if let Some(channel) = &self.channel {
            *channel.checkpoint.lock().unwrap() = Some(checkpoint);
        }
    }

    pub fn clear_checkpoint(&self) {
        if let Some(channel) = &self.channel {
            *channel.checkpoint.lock().unwrap() = None;
        }
    }

    /// Return the reusable channel, opening a new one when needed.
    pub async fn connect(&mut self, url: &str, headers: &[(String, String)]) -> Result<()> {
        if self.http_fallback() {
            return Err(anyhow!("session transport is pinned to HTTP"));
        }
        let affinity = affinity(url, headers);
        let current = self.channel.clone();
        if let Some(current) = current {
            let expired = current.opened_at.elapsed() >= ROTATE_AFTER;
            if current.affinity == affinity && !expired {
                return Ok(());
            }
            self.close_channel().await;
        }
        let socket = connect(url, headers).await.map_err(|error| {
            self.state.inner.lock().unwrap().http_fallback = true;
            error
        })?;
        let channel = Arc::new(Channel {
            socket: AsyncMutex::new(Some(socket)),
            affinity,
            opened_at: Instant::now(),
            checkpoint: Mutex::new(None),
        });
        self.state.inner.lock().unwrap().channel = Some(channel.clone());
        self.channel = Some(channel);
        Ok(())
    }

    pub async fn send(&self, frame: String) -> Result<()> {
        let Some(channel) = &self.channel else {
            return Err(anyhow!("no active WebSocket channel"));
        };
        let mut socket = channel.socket.lock().await;
        let ws = socket.as_mut().ok_or_else(|| anyhow!("WebSocket channel is closed"))?;
        ws.send(Message::text(frame)).await.map_err(|error| anyhow!("{error}"))
    }

    pub async fn recv(&self) -> Result<Option<Message>> {
        let Some(channel) = &self.channel else {
            return Ok(None);
        };
        let mut socket = channel.socket.lock().await;
        let Some(ws) = socket.as_mut() else {
            return Ok(None);
        };
        match tokio::time::timeout(IDLE_TIMEOUT, ws.recv()).await {
            Ok(Some(message)) => Ok(Some(message?)),
            Ok(None) => Ok(None),
            Err(_) => Err(anyhow!("timed out waiting for WebSocket data")),
        }
    }

    pub async fn close_channel(&mut self) {
        let channel = self.state.inner.lock().unwrap().channel.take();
        self.channel = None;
        if let Some(channel) = channel {
            let socket = channel.socket.lock().await.take();
            if let Some(socket) = socket {
                let _ = socket.close(1000, "").await;
            }
        }
    }

    pub fn pin_http(&mut self) {
        self.state.inner.lock().unwrap().http_fallback = true;
    }

    /// A socket/protocol failure. Repeated failures eventually pin the session to HTTP.
    pub async fn poison(&mut self) {
        self.close_channel().await;
        let mut inner = self.state.inner.lock().unwrap();
        inner.stream_failures += 1;
        if inner.stream_failures >= MAX_STREAM_FAILURES {
            inner.http_fallback = true;
        }
    }

    pub fn finish(&mut self, success: bool) {
        if success {
            self.state.inner.lock().unwrap().stream_failures = 0;
        }
    }
}

async fn connect(url: &str, headers: &[(String, String)]) -> Result<wreq::ws::WebSocket> {
    let mut builder = crate::http::client().websocket(url);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let response = tokio::time::timeout(CONNECT_TIMEOUT, builder.send())
        .await
        .map_err(|_| anyhow!("timed out opening WebSocket"))?
        .map_err(|error| anyhow!("WebSocket handshake failed: {error}"))?;
    tokio::time::timeout(CONNECT_TIMEOUT, response.into_websocket())
        .await
        .map_err(|_| anyhow!("timed out upgrading WebSocket"))?
        .map_err(|error| anyhow!("WebSocket upgrade failed: {error}"))
}

pub async fn close(session_id: &str) {
    let state = states().lock().await.remove(session_id);
    let Some(state) = state else { return };
    state.inner.lock().unwrap().closed = true;
    let channel = state.inner.lock().unwrap().channel.take();
    if let Some(channel) = channel {
        let socket = channel.socket.lock().await.take();
        if let Some(socket) = socket {
            let _ = socket.close(1000, "").await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_ignores_trace_headers() {
        let first = affinity(
            "wss://api.example/v1/responses",
            &[("authorization".into(), "Bearer x".into()), ("b3".into(), "one".into())],
        );
        let second = affinity(
            "wss://api.example/v1/responses",
            &[("authorization".into(), "Bearer x".into()), ("b3".into(), "two".into())],
        );
        assert_eq!(first, second);
    }

    #[test]
    fn affinity_changes_with_auth() {
        let first = affinity("wss://api.example/v1/responses", &[("authorization".into(), "Bearer x".into())]);
        let second = affinity("wss://api.example/v1/responses", &[("authorization".into(), "Bearer y".into())]);
        assert_ne!(first, second);
    }
}

pub async fn close_all() {
    let all = {
        let mut states = states().lock().await;
        states.drain().map(|(_, state)| state).collect::<Vec<_>>()
    };
    for state in all {
        state.inner.lock().unwrap().closed = true;
        let channel = state.inner.lock().unwrap().channel.take();
        if let Some(channel) = channel {
            let socket = channel.socket.lock().await.take();
            if let Some(socket) = socket {
                let _ = socket.close(1000, "").await;
            }
        }
    }
}
