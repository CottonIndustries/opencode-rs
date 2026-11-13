---
name: app-e2e-tests
description: Use when writing, changing, or debugging end-to-end tests for the opencode-rs CLI, HTTP server, or TUI. Covers isolated test homes, mock providers and WebSocket servers, waiting for readiness, process cleanup, Windows timing, and the boundary between e2e smoke tests and cargo unit tests.
---

# End-to-end tests

End-to-end tests exercise the real binary: argument parsing, config loading, provider requests, tools, session persistence, server routes and terminal output. Keep them few and user-visible. Protocol details belong in focused Rust tests; TUI drawing belongs in `src/tui/tests.rs`.

## What belongs here

- `opencode run` admits a prompt, calls a provider, writes the expected session file and exits.
- `opencode serve` answers its routes, streams events and enforces auth.
- A plugin loads in Deno and its tool or hook changes behavior end to end.
- `opencode auth`, `plugin`, `mcp`, `session`, `workspace`, `service` and `debug` commands mutate the right config or storage.
- A TUI startup path renders without requiring a real terminal.

Do not put every edge case in an e2e test. A single happy path plus one failure path per feature is usually enough.

## Isolate everything

Never use the developer's real home, credentials, storage or ports:

```powershell
$root = "$env:TEMP\opencode\e2e-<name>"
Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$root\home\.config\opencode", "$root\project" | Out-Null
'{}' | Set-Content "$root\home\.config\opencode\opencode.json"
$env:OPENCODE_TEST_HOME = "$root\home"
```

Use a fresh root per test file. Set `OPENCODE_DISABLE_PLUGINS=1` unless plugins are the subject. Set `OPENCODE_DENO` to the vendored binary when the test must not download Deno.

## Build once, run many

Build outside the test body:

```powershell
cargo build
$exe = ".\target\debug\opencode.exe"
```

Inside a Rust integration test, use `env!("CARGO_BIN_EXE_opencode")` so Cargo supplies the built binary path.

## Mock the provider, not the engine

A Deno mock server is the cheapest way to capture and script provider traffic:

```ts
const port = Number(Deno.args[0])
Deno.serve({ port }, async (request) => {
  console.log(JSON.stringify({ url: request.url, headers: [...request.headers], body: await request.text() }))
  const body = [
    `data: ${JSON.stringify({ choices: [{ delta: { content: "hello" }, finish_reason: null }] })}\n\n`,
    `data: ${JSON.stringify({ choices: [{ delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } })}\n\n`,
    "data: [DONE]\n\n",
  ].join("")
  return new Response(body, { headers: { "content-type": "text/event-stream" } })
})
```

Point a throwaway provider at it through `opencode.json`:

```json
{
  "provider": {
    "local": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:<port>", "apiKey": "test" },
      "models": { "test": { "name": "Test", "limit": { "context": 10000, "output": 1000 } } }
    }
  }
}
```

Use a free port chosen by the test (`Deno.serve({ port: 0 })` and print the address), not a hard-coded one. Never require real API keys.

## Wait for a condition, not a clock

Start the server or mock, then poll for readiness:

```powershell
$deadline = (Get-Date).AddSeconds(15)
do {
  try { Invoke-RestMethod "http://127.0.0.1:$port/api/location" | Out-Null; break } catch { Start-Sleep -Milliseconds 50 }
} while ((Get-Date) -lt $deadline)
```

The same rule applies to files, sockets and process exit. A fixed `Start-Sleep 3` is a flake waiting to happen on a loaded runner.

## Stop children in finally

Windows keeps handles and ports alive after a failed assertion. Track every `Start-Process` and stop it:

```powershell
$server = Start-Process $exe -ArgumentList @("serve", "--port", "$port", "--dir", "$root\project") -PassThru -WindowStyle Hidden
try {
  # assertions
} finally {
  Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
}
```

Give the process a chance to flush output before reading its log. Avoid `Wait-Process` forever; use a bounded wait.

## Assertions

- Assert the request body and headers when a provider or server contract is the point.
- Assert files, session records, exit codes and visible text, not log formatting.
- For SSE, parse complete `data:` frames; do not assume chunk boundaries.
- For TUI smoke, assert a few stable substrings rather than a full screen dump unless the test is explicitly about layout parity.
- Keep every assertion tied to a user-visible or protocol-visible outcome.

## Windows notes

- Process creation is expensive; one server per file is better than one per test.
- Canonicalize paths before comparing; `\\?\` prefixes and 8.3 names show up in Windows output.
- A port can be taken by another process between check and bind. Prefer `--port 0` and read the bound address.
- Do not depend on SIGTERM semantics; `Stop-Process` is the portable cleanup path.

## Budget

Keep the e2e suite out of `cargo test` unless it is hermetic and fast. Shell/Deno smoke scripts belong under `tools` or `tests` fixtures with an explicit command. A slow e2e suite that is not run is worse than a focused one that is.
