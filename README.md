# opencode (Rust)

A Rust rewrite of the OpenCode terminal client, ported from the bundled v2
sources extracted from `opencode.exe` (`../extracted`, `../readable`). It is a
single binary (~20 MB release) that uses the Microsoft Visual C++ Redistributable
on Windows, with no Bun runtime or background server needed.

```
cargo build --release
./target/release/opencode            # interactive TUI in the current directory
./target/release/opencode run "fix the failing test"
```

## Workspace layout

The project is a Cargo workspace. The binary is a thin shell around three
library crates:

| Crate | Responsibility |
|---|---|
| `crates/opencode-foundation` | Paths, utilities, HTTP fingerprinting, credentials, account usage and shared state |
| `crates/opencode-storage` | Sessions, SQLite stores, durable events, snapshots, media, artifacts, VCS, sharing and statistics |
| `crates/opencode-services` | Hosted Console config, well-known manifests, forms and saved permissions |
| `crates/opencode-core` | Configuration, models, providers, agent engine, tools, plugins, jobs, schedules and workspaces |
| `crates/opencode-tui` | Ratatui terminal UI, themes, transcript rendering and the plugin layout renderer |
| `crates/opencode-server` | HTTP API, SSE event feed and PTY WebSocket transport |

`src/` keeps only the binary entry point, CLI parsing, non-interactive `run` and
ACP. Build and test everything with `cargo build --workspace` and
`cargo test --workspace`.

The remaining `opencode-core` crate is deliberately one dependency cluster:
`app`, `engine`, `config`, `plugin`, `provider` and `tools` are mutually
recursive. Splitting those further requires explicit trait boundaries rather
than file moves.

BoringSSL is built from source, which needs CMake, NASM and libclang. `.cargo/config.toml`
points at `tools/cmake`, `tools/nasm` and `tools/libclang` (not committed); change those paths
if yours live elsewhere.

## Commands

| Command | Description |
|---|---|
| `opencode [dir]` / `opencode mini` | Interactive TUI (`-m provider/model`, `--agent`, `-c`, `-s <id>`, `--fork`, `--prompt`) |
| `opencode run [message..]` | Non-interactive run. `-m provider/model#variant`, `--agent`, `-c`, `-s`, `--fork`, `-f file`, `--title`, `--thinking`, `--format json`, `--auto-approve`. Piped stdin is appended to the message. A leading `/command` expands that command. |
| `opencode acp [--login]` | Agent Client Protocol server on stdio for editors such as Zed (`--login` runs `auth login`) |
| `opencode serve [--hostname H] [--port P] [--password P] [--dir D]` | Local HTTP API, SSE event feed, and WebSocket PTY streaming (`/api/pty/:id/connect`). Basic auth is enabled when a password is set or `OPENCODE_SERVER_PASSWORD` is present. |
| `opencode models [provider] [--verbose] [--refresh]` | List models from providers with credentials |
| `opencode auth list\|login\|logout\|export\|import\|switch\|sync` | Manage API keys and OAuth logins: ChatGPT, GitHub Copilot, and OpenCode Console device login (`opencode auth login opencode [--server URL]`); `sync` refreshes hosted Console config, instructions and skills |
| `opencode session list\|delete\|export\|import` | Manage sessions (`--sanitize` on export) |
| `opencode stats [--days N] [--json]` | Token, cost, model and tool statistics |
| `opencode mcp list` | MCP server status |
| `opencode service start\|stop\|status` | Run the HTTP server as a detached local service |
| `opencode api METHOD PATH` | Call a running server |
| `opencode workspace list\|create\|remove` | Manage logical workspaces and local environment bindings |
| `opencode repository cache` | Clone or refresh a remote repository cache |
| `opencode migrate` | Index legacy session history |
| `opencode mcp add <name> [--url U] [--header K=V] [--env K=V] [--] <cmd...>` | Add a remote or local MCP server |
| `opencode mcp auth <name>` / `mcp logout <name>` | Authorize or forget a remote MCP server with OAuth; the TUI /mcps dialog starts the same flow, shows the sign-in link, and opens it with enter |
| `opencode plugin list\|add\|remove\|check\|update` | Manage configured plugins; `check` compares pinned npm targets with the registry and `update` rewrites them |
| `opencode pair [--url U]` | Create a single-use pairing link for a running server (`/auth/connect/:code`) |
| `opencode skills` | List available built-in, project and global skills |
| `opencode debug paths\|config\|agents\|skills\|prompt\|plugins` | Troubleshooting (`plugins` also lists plugin tools, TUI commands, pages and slots) |
| `opencode debug plugin-tool <name> [json]` | Execute a plugin tool with the given input |
| `opencode debug plugin-tui <name> [input]` | Execute a plugin TUI command |
| `opencode debug events [session]` | Show durable events from the SQLite event log |

## What is ported

- **Providers** via the models.dev catalog (snapshot embedded, refreshed into
  `~/.cache/opencode/models.json`) with four native streaming protocols:
  Anthropic Messages (adaptive thinking + effort, signatures round-tripped,
  prompt caching, eager tool-input streaming on first-party), OpenAI Responses
  (encrypted reasoning round-trip), OpenAI-compatible Chat Completions (OpenRouter,
  Groq, DeepSeek, xAI, local servers, `interleaved` reasoning fields) and Gemini
  (thought signatures). Retries with backoff on 429/5xx (not on quota errors).
  `#variant` selects reasoning effort/budget. OpenAI Responses can use the
  WebSocket transport (`provider.<id>.options.transport: "websocket"`) with the
  same BoringSSL/Bun HTTP stack, for primary step requests. One socket is
  reused per session, rotated after 55 minutes, with a 15 second connect timeout
  and 30 minute idle timeout.
  Handshake failures pin the session to HTTP/SSE, repeated stream failures do the
  same after five attempts, and `previous_response_id` continuations send only
  the new input when the prefix still matches.
- **OpenCode Zen free tier**: with no OpenCode key, the `opencode` provider uses the
  anonymous `public` key and lists only free models. Model requests match the TypeScript
  client: `User-Agent: opencode/latest/2.2.7/cli`, `x-opencode-client: cli` (or
  `OPENCODE_CLIENT`), its session/affinity/project headers and ID formats, trace headers,
  `max_completion_tokens`, and its title request. Set `OPENCODE_API_KEY` to unlock paid Zen models.
- **Network fingerprint** (`crates/opencode-core/src/http.rs`): all HTTP goes through BoringSSL configured like
  Bun 1.4.4, so the ClientHello (ciphers, groups, key shares, signature algorithms, extension
  order, ALPN `http/1.1`) is the TypeScript client's, and headers are written as Bun's `fetch`
  writes them: sorted, Bun's casing, joined repeats, then `Connection`, `Accept`, `Host`,
  `Accept-Encoding`, `Content-Length`. `NODE_TLS_REJECT_UNAUTHORIZED=0` disables certificate
  checks, as in Bun.
- **Credentials**: env vars from the catalog, `provider.<id>.options.apiKey`,
  `auth.json`, and API keys saved by the TypeScript client in `opencode.db`
  (read-only).
- **Config**: the same global, project and system locations as upstream opencode:
  `opencode.json`/`.jsonc` in `~/.config/opencode` (or `OPENCODE_CONFIG_DIR`),
  `%ProgramData%\opencode` / `/etc/opencode` / `/Library/Application Support/opencode`,
  the project chain down to `.opencode/`, `OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`,
  and upstream's V2 plural keys (`providers`, `agents`, `commands`, `permissions`,
  `mcp.servers`, `skills.paths`/`skills.urls`) are normalized. Supports
  `{env:}`/`{file:}` substitution, custom providers/models, `agent`, `command`,
  `permission`, `mcp`, `tools`, `instructions`, `skills` (local paths and URL
  sources), `lsp`, `compaction`, `formatter`, `experimental.todo`,
  `experimental.schedules` and `experimental.codemode`. The credential database
  honours `OPENCODE_DB` and defaults to `<data>/opencode.db`, so existing
  upstream API keys, OAuth credentials and MCP OAuth tokens keep working.
  Skills are discovered from each config root's `skill/` and `skills/` folders,
  project `.opencode/`, and the shared `~/.claude` / `~/.agents` roots,
  including upstream's `SKILL.md` or root `.md` layouts and URL skill sources.
- **Agents**: `build`, `general`, `explore` (+ hidden `title`, `summary`,
  `compaction`) and custom agents from config or `agent/*.md`.
- **Tools** with the original descriptions and schemas: `shell` (Git Bash /
  PowerShell on Windows, timeouts, background jobs with completion
  notifications, command-prefix "always allow" patterns), `read` (text/images/PDF,
  directories, nested `AGENTS.md`), `write`, `edit` (quote/dash/whitespace-
  tolerant matching, CRLF and BOM preservation), `patch` (used instead of
  edit/write for GPT models), `glob`, `grep` (gitignore-aware; omitted for
  Claude/GPT as upstream does), `webfetch`, `websearch` (Exa/Tavily/Parallel/Firecrawl
  keys), `todowrite`, `question`, `subagent` (foreground/background child
  sessions), `skill`, artifacts, and MCP tools (stdio and streamable HTTP,
  including resources and prompt commands). With `experimental.schedules`, the
  `schedule_*` tools wake a session on a delay, time, interval, cron, or a shell
  condition. With `experimental.codemode`, the `execute` tool runs JavaScript in
  Deno with `tools.*` calls back into the session tool registry. LSP servers
  from `lsp` report diagnostics after edits.
- **Permissions**: last-match-wins rules (`allow`/`ask`/`deny`), external
  directory checks, `.env` protection, per-session "always" approvals.
- **Engine**: streaming agent loop, parallel tool execution, steering (messages
  typed while busy are folded into the next step), max steps, output-length
  recovery, automatic and manual compaction with the upstream checkpoint
  template, title generation, cost tracking.
- **System prompt**: upstream model-specific prompts (`assets/prompts`), tool
  guidance, environment block, date, AGENTS.md / `instructions`, skills list.
- **ACP** (`opencode acp`): the upstream agent for Zed and other ACP editors — sessions
  (new, load with history replay, resume, fork, list, delete, close), model / effort / mode
  selectors, streamed text, thinking and tool calls with locations and before/after diffs,
  permission prompts on the tool call, cancel, `/compact` and custom slash commands,
  embedded context and images, client MCP servers (stdio, HTTP, and client-hosted WebSocket), usage updates.
  Requests identify as `acp`, as upstream does in this mode. Zed setup:
  `"agent_servers": { "OpenCode (Rust)": { "type": "custom", "command": "C:/path/to/opencode.exe", "args": ["acp"] } }`
- **Plugins** (V2 and legacy V1): a Deno sidecar loads TypeScript/JavaScript
  plugins, exposes the `@opencode/plugin` V2 Promise and Effect APIs, and
  bridges them to the Rust engine. Configured with `plugins` (V2:
  `{ "package": "...", "options": {} }`) or `plugin` (V1 string / `[string,
  options]`) and auto-discovered from `plugin/`/`plugins/` in each config
  directory. Supports:
  ```json
  {
    "plugins": ["./.opencode/plugin/local.ts", { "package": "some-plugin", "options": {} }]
  }
  ```
  - V2 `{ id, setup }` and `{ id, effect }` default exports, plus V1 plugin
    exports, `ctx.options`, and V1 `$`/`client` inputs.
  - Transform hooks for agents, commands, skills, tools, providers, models,
    MCP servers, websearch/VCS/worktree/reference/integration/artifact
    registries.
  - Runtime hooks: `session.prompt`, `session.context`, `session.headers`,
    `session.compaction`, `session.compacting` (V1), `session.autocontinue`
    (V1), `session.title`, `session.text.complete`, `session.model.resolve`,
    `session.model.request`, `session.experimental.idle`,
    `experimental.ws.handshake`, `experimental.ws.send`,
    `experimental.ws.receive`, `tool.execute.before`, `tool.execute.after`,
    `tool.definition` (V1), `shell.create.before`, `permission.evaluate`, and
    event subscriptions.
  - Custom tools (Zod, Effect Schema, Standard Schema or JSON Schema inputs)
    with `ctx.ask`, `ctx.metadata`/`ctx.progress`, abort signals, and plugin
    commands.
  - A file-backed `ctx.storage`, `ctx.event.subscribe`, permission
    list/get/reply/assert, session operations, and the common client API.
  - `OPENCODE_DENO` selects a Deno binary; otherwise the pinned release is
    downloaded once into the opencode cache. Local plugin dependencies are
    resolved from their `package.json` without writing `node_modules` into the
    project. `OPENCODE_DISABLE_PLUGINS=1` skips loading plugins.
  - Package `./tui` entries load in TUI mode with `ctx.keymap`, command palette
    and slash commands, key bindings, toasts, dialogs, persistent storage,
    router pages, session panels and footer/sidebar slots. Slot and page JSX is
    reduced to a layout tree and rendered by ratatui. Compiled Solid JSX
    plugins run through a Solid-compatible runtime and a flexbox-style Rust
    layout engine with row/column placement, basis, grow/shrink, wrap,
    align/justify, padding, borders, overflow clipping and scrolling. Markdown
    and code blocks use the native markdown and syntax renderers; inputs,
    textareas and selects are editable and focused from the keyboard; pages
    route keys and paste events back to plugin handlers and support
    `requestRender` animation ticks. This remains a renderer subset rather than
    a port of the native OpenTUI engine, so uncommon widgets and mouse text
    selection still fall back to text.
- **TUI** (ratatui): markdown rendering, thinking blocks, tool calls with diffs,
  permission dialog, model/session/agent pickers, command palette, slash
  commands (`/new /clear /sessions /open /models /agents /usage /mcps /variants
  /connect /settings /status /themes /help /exit /rename /timeline /fork
  /compact /undo /redo /copy /export /cd /editor /skills /worktrees /terminal
  /plugins /diff /stats /btw /reload /debug /pair /restart /highlights /update`
  plus config/plugin commands), `@file` attachments, `@skill` mentions, prompt
  history, queued messages, a persistent terminal dialog, diff and stats views,
  workspace switching, side questions and config reload. Project skills live in
  `.opencode/skills/<name>/SKILL.md`, matching upstream's default skill set.

Sessions remain readable as JSON in `~/.local/share/opencode/storage/session/<project>/`
and are indexed in `storage/sessions.db` for loading and listing. Durable
events live in `storage/events.db`; the old `events.jsonl` log is imported on
first use. Both databases migrate existing files automatically and run in WAL
mode. Sessions can bind a local workspace, jobs and background work are stored
in `storage/jobs.db`, snapshots are content-addressed under
`storage/snapshots`, configuration files are watched and reloaded when no
plugins are active, and configuration policies are enforced before tool
permissions. Generated images are content-addressed under `storage/media` and served
at `/api/experimental/media/:hash` only while a stored message, event, or
inbox item still references them. Session messages are also normalized into the
`messages` table for querying.

## Not ported

Hosted integration sources from `/.well-known/opencode` and OpenCode Console
config, instructions and skills are cached under `storage` and merged below
local configuration. Things in the original that depend on domains this port
does not implement: `github`, `upgrade`, and Claude Pro/Max OAuth. ChatGPT,
GitHub Copilot, and OpenCode Console device logins are available through
`opencode auth login`. Bedrock/Vertex/Azure still need an OpenAI-compatible
`baseURL`. Shell permission parsing uses a quote-aware splitter instead of
tree-sitter. Code Mode uses Deno with a `tools.*` bridge rather than the
upstream bytecode VM; snapshots, the `search` helper and static tool
authorization are not part of it. Remote workspace/environment drivers and the
full native OpenTUI/Solid layout engine is replaced by a renderer subset with flexbox-style layout, scrolling and styled text.

Artifacts, snapshots/undo, post-edit formatters, VCS/worktree strategies and
RPC endpoints are available through the API and plugin host. `opencode serve`
covers sessions (prompt, command, shell, fork, revert, compact, diff, forms,
inbox, permissions, environment, move, generate), messages, agents, providers,
models, projects, plugins, MCP (including add/connect/disconnect and client
WebSocket attachments), filesystem
read/list/find/write, background shells and jobs, scheduled tasks, web search,
image/decision generation with media blobs, artifacts, PTY and persistent
terminals, Console usage limits, realtime
voice sessions, VCS and worktrees, durable session logs and V2 events over SSE,
plugin references, and
pairing/auth. Integrations and credentials are handled by plugins and exposed
through their HTTP routes; `aisdk` SDK/language hooks pass options through, but
a plugin-returned model object cannot replace the Rust provider.

## Tests

`cargo test` covers wildcard/permission evaluation, JSONC and frontmatter
parsing, edit matching (CRLF, typographic, trailing whitespace), patch parsing,
shell command splitting, DB credential import, upstream ID formats, ACP prompt
conversion and diff reconstruction, input editing and TUI rendering.
