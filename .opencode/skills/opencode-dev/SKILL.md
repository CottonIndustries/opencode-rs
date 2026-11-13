---
name: opencode-dev
description: Use when running, debugging, or verifying opencode-rs itself. Covers building the Rust binary, driving the CLI/TUI, exercising the HTTP server, inspecting logs and storage, working on the Deno plugin host, and isolating smoke tests from the real user environment.
---

# Developing opencode-rs

opencode-rs is a Rust port of opencode v2. The reference TypeScript tree lives in `reference/opencode` and is read-only material for parity work; never run its package scripts as part of this repo's verification.

## Repository map

- `src/main.rs` - clap command surface and startup.
- `src/app.rs` - application assembly, registries, plugins, config reload.
- `src/engine.rs` - session turns, provider requests, tools, events.
- `src/session.rs` - transcript model and persistence.
- `src/provider/` - protocol implementations, media, OAuth, WebSocket transport.
- `src/tools/` - built-in tools and skill discovery.
- `src/server.rs` - `opencode serve` HTTP API, SSE, WebSocket PTY and pairing.
- `src/plugin/mod.rs` and `src/plugin/host.ts` - Rust/Deno plugin bridge.
- `src/tui/` - ratatui client, plugin dialogs, pages, slots and layouts.
- `assets/` - built-in prompts, models and themes.
- `.opencode/skills/` - project skills for working on this port.

## Build and verify

```powershell
cargo build
cargo test
cargo check
```

Run `cargo test <filter>` for a focused loop, for example `cargo test provider::openai_chat`. The full suite is fast enough to run before a commit. `deno fmt --check src/plugin/host.ts` when editing the host.

The debug binary is `target\debug\opencode.exe` on Windows and `target/debug/opencode` elsewhere.

## Isolate development

Every manual run should use a throwaway home so credentials, sessions and storage stay out of the real profile:

```powershell
$root = "$env:TEMP\opencode\dev"
Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$root\home\.config\opencode", "$root\project" | Out-Null
'{}' | Set-Content "$root\home\.config\opencode\opencode.json"
$env:OPENCODE_TEST_HOME = "$root\home"
$env:OPENCODE_DISABLE_PLUGINS = "1"
```

Use `OPENCODE_DISABLE_PLUGINS=1` when isolating core behavior. Omit it when testing plugin loading, and keep `OPENCODE_DENO` pointed at the vendored Deno binary when auto-download is undesirable.

## CLI and TUI

```powershell
target\debug\opencode.exe run --dir $root\project --model <provider/model> "hello"
target\debug\opencode.exe "$root\project"    # interactive TUI
```

Useful inspection commands:

```powershell
target\debug\opencode.exe debug paths
target\debug\opencode.exe debug config
target\debug\opencode.exe debug skills
target\debug\opencode.exe debug agents
target\debug\opencode.exe debug prompt
target\debug\opencode.exe debug plugins
target\debug\opencode.exe debug events <session-id>
```

The TUI has a `/skills` command that inserts a skill mention, and `@skill` completion in the prompt. Plugin commands, pages, dialogs and slots are listed by `debug plugins`.

## HTTP server

```powershell
target\debug\opencode.exe serve --port 0 --dir $root\project
```

The startup line prints the bound address. Authenticate with `OPENCODE_SERVER_PASSWORD` and HTTP Basic as `opencode:<password>` when configured. Smoke the surface with `/api/location`, `/api/session`, `/api/event` and `/api/pty`.

For server scripts, start the process once, poll the printed address or `/api/location` until it answers, run the assertions, and stop the process in `finally`. Do not use fixed sleeps as the readiness signal.

## Plugin development

`src/plugin/host.ts` runs under Deno and is embedded into the binary at build time. Keep the bridge line-delimited JSON-RPC and the V2 envelope shapes stable.

- `state.changed` pushes plugin registry changes to Rust.
- `callRPC`/`request` dispatch into `src/plugin/mod.rs`.
- V1 plugins receive the compatibility client; V2 plugins use the Effect/Promise API.
- `debug plugin-tool <name> <json>` exercises a plugin tool without opening the TUI.
- Deno host changes are not picked up until the Rust binary is rebuilt and restarted.

## Logs and storage

`debug paths` prints the resolved directories. State lives under the data directory:

- `log` (a sibling of `storage`) - process logs.
- `storage/sessions.db` - session index and normalized messages.
- `storage/events.db` - durable event log.
- `storage/snapshots` - content-addressed snapshots.
- `storage/media` - generated media blobs.
- `storage/jobs.db` - background jobs.

Use `OPENCODE_RESUME_BACKGROUND` only when a long-running mode should replay unconsumed jobs.

## Parcels and boundaries

- No desktop or web app is ported. TUI plugin JSX is rendered through a small box/text layout shim, not OpenTUI/Solid.
- Providers that need cloud SDK authentication (Bedrock, Vertex) are not native; Azure uses its deployment URL shape.
- Config hot reload rebuilds providers, agents, skills, commands and MCP from disk, except while plugins are active; restart for plugin changes.

## Verification habits

- Reproduce before fixing; capture the outgoing provider request or server response instead of guessing.
- Prefer a failing focused test, then the full suite.
- Check both a plugin-enabled and plugin-disabled path when touching hooks.
- Treat `reference/opencode` as evidence, but validate behavior through this repo's Rust code and tests.
