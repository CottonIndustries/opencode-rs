---
name: component-tests
description: Use when testing one component in isolation in opencode-rs: a tool, provider protocol, plugin domain, server handler, or TUI surface. Covers small Rust tests, mock HTTP/WebSocket peers, temp projects, TestBackend rendering, fixtures, and cleanup.
---

# Component tests

A component test mounts one seam and drives it with controlled inputs. It is faster and more diagnostic than an end-to-end test, and it should not need a real provider key, the developer's home, or a listening public port.

## Choose the seam

| Component | Seam | Harness |
| --- | --- | --- |
| Tool | `Tool::execute` with `Context` | temp project + `App::init` |
| Provider protocol | `provider::*::stream` or `post` | local mock server + event channel |
| Plugin domain | `plugin::client_call` / Deno host | temp home + minimal plugin |
| Server handler | `server::route` or a live server | `Request` value or port 0 |
| TUI | `Ui::draw` | `ratatui::backend::TestBackend` |
| Session logic | `Session` / `Handle` | in-memory session + temp storage |

Prefer the smallest layer that still exercises the behavior. A request-building test should not spawn a server; a route-auth test should not call a model.

## Tools

Build a temporary app, resolve the session's agent and model, then run the tool through its real registry:

```rust
let dir = std::env::temp_dir().join(format!("opencode-tool-{}", crate::util::id("")));
std::fs::create_dir_all(&dir)?;
let app = crate::app::App::init(dir.clone()).await?;
let agent = app.agent(&app.default_agent())?.clone();
let selection = crate::engine::select_model(&app, &agent, &Default::default(), None)?;
let tools = crate::tools::registry(&app, &agent, &selection.model).await;
let tool = tools.into_iter().find(|tool| tool.name() == "read").unwrap();
let out = tool.execute(serde_json::json!({ "path": "notes.txt" }), &ctx).await?;
```

Assert the output and the filesystem effect. Clean the temp directory even when the assertion fails; a guard type or `let-else` with explicit cleanup is fine in tests.

## Provider protocols

Use a local HTTP server that records the request and returns a scripted SSE stream. Assert:

- URL, method, auth header and body fields.
- Parsed events, tool-call fragments, usage and finish reason.
- Error mapping for non-2xx responses and malformed frames.

For WebSocket transport, use a local server that accepts one upgrade, records frames and can reject or close mid-stream. Do not point tests at a public endpoint.

## Plugin components

A plugin test should include only the surface under test:

- Registry transforms: call `apply_*`/`client_call` with a JSON state snapshot.
- Host hooks: a minimal Deno plugin that registers one hook and records its input.
- Reload/state propagation: send `state.changed` and assert Rust re-derives the affected registry.

Keep the plugin source inline in the test home or in a small fixture directory. Do not depend on an npm install for a unit test.

## Server handlers

Call the route function directly when possible. For transport behavior (auth, SSE, WebSocket, body limits), start `serve` on port 0 and use a real client. Assert status, JSON shape and headers, then stop the server.

## TUI surfaces

Use `TestBackend` and assert stable text, not exact padding:

```rust
let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
terminal.draw(|frame| ui.draw(frame)).unwrap();
let screen = dump(&terminal);
assert!(screen.contains("Skills"));
```

For dialog behavior, call the same `dialog_select`/`exec` entry points the key handler uses. Do not duplicate rendering logic in the test.

## Fixtures and cleanup

- Use unique temp directories per test, not a shared fixed path.
- Remove state before and after; Windows can keep handles open.
- Reset global environment variables with a guard or serialize the tests that need them.
- Never leave child processes or mock listeners running.
- Keep fixture files small and prefer inline JSON over hidden shared state.

## What not to assert

- Exact timestamps, durations or line wrapping.
- Full log text.
- Internal struct layout.
- Network reachability of a third-party service.
- Ordering that the product does not promise.

## Verification loop

```powershell
cargo test component_name
cargo test
```

A component test that needs a running TUI, a real provider or a fixed port is an e2e test; move it to the e2e skill and mark it explicitly.
