---
name: unit-tests
description: Use when writing, reviewing, speeding up, or fixing Rust unit tests in opencode-rs. Covers cargo test filters, temp homes, environment isolation, mock HTTP/WebSocket peers, deterministic waits, Windows path and process behavior, serial tests, and the line between unit tests and smoke scripts.
---

# Fast, reliable unit tests

opencode-rs uses Rust's built-in test harness. Tests are colocated with the module (`#[cfg(test)] mod tests`) or live under `tests/` for binary-level integration. The whole suite is currently fast; keep it that way.

## Run

```powershell
cargo test
cargo test provider::openai_chat
cargo test tui::tests::skills_command -- --nocapture
cargo test -- --test-threads=1   # only for tests that share global state
```

`cargo check` first for a tight loop when only types changed.

## Isolate the world

- Use a unique temp directory per test or per fixture, not a fixed path.
- `OPENCODE_TEST_HOME` is read once by `paths::get()`, whose result is cached. Setting it inside an already-running test does not move paths. Use it for child processes, or spawn a subprocess for home-isolated integration tests.
- In-process tests should use unique project directories and avoid config/auth/storage paths they do not need.
- Never read the developer's real home, auth file or storage.
- Clear `OPENCODE_DISABLE_PLUGINS`/`OPENCODE_DENO` deliberately; restore them afterwards.
- Remove temp directories in `Drop` guards or at the end of the test body. Windows can fail to remove a directory while a file handle or child is open.

```rust
struct Home(std::path::PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
```

## Mock the boundary

- Provider tests: local HTTP server, scripted SSE body, assert URL/headers/body and emitted events.
- WebSocket tests: local upgrade server or an in-process frame decoder; do not use a public endpoint.
- MCP tests: stdio fixture or a local WebSocket server.
- Server tests: inside `src/server.rs`, call the private `route` directly; from another module, bind port 0 for transport behavior.
- Tool tests: temp project plus `App::init`; assert output and filesystem effect.

No test should require a real API key or network access.

## Control time

- Do not use `sleep` to wait for a result; poll a condition with a short deadline.
- For long timeouts in product code, make the duration injectable or use a test-only small value.
- Avoid asserting duration or wall-clock speed. Assert ordering, counts and final state.
- Tokio tests: use `#[tokio::test]` and yield to the runtime instead of sleeping when the work is cooperative.

## Determinism

- Avoid fixed ports; use `port: 0` and read the bound address.
- Sort collections before asserting when the product does not promise order.
- Normalize Windows paths (`\\?\`, separators, case) before comparing.
- Build JSON with `serde_json::json!` and compare values, not formatting.
- When a test depends on a process exit, wait for the child and assert its status, not a log line.

## Windows behavior

- Process creation is expensive; keep one child per test file where possible.
- File rename over an open handle fails; close readers and writers first.
- `Stop-Process` is not SIGTERM; do not assert POSIX signal semantics.
- Ephemeral ports come from 49152 and up; never hard-code one.
- Canonicalize both sides when comparing paths.

## Test shape

- Name the behavior, not the implementation: `applies_plugin_message_transform`, not `test_apply`.
- Arrange one fixture, act once, assert the smallest user-visible or protocol-visible outcome.
- A regression test should fail for the bug before the fix and pass after it.
- Keep one contract per test file; don't assert three unrelated layers in one test.

## Do not

- Add `--test-threads=1` to make a flaky test pass; remove the shared state instead.
- Mock the module under test.
- Assert internal struct debug output.
- Depend on test execution order.
- Hide a race with a larger timeout.

## Smoke scripts

Shell/Deno smoke tests that exercise the built binary belong under `tools` or `tests` fixtures and run explicitly. They are not a substitute for a focused Rust test, and they should use an isolated `OPENCODE_TEST_HOME`.

## Before pushing

```powershell
cargo test
cargo build
```

If the change touches the plugin host, also run `deno fmt --check src/plugin/host.ts`. Review the diff for `sleep`, fixed ports, real credentials and temp paths that are not cleaned up.
