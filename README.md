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
| `opencode auth list\|login\|logout` | Manage API keys (`<data>/auth.json`) |
| `opencode session list\|delete\|export\|import` | Manage sessions (`--sanitize` on export) |
| `opencode stats [--days N] [--json]` | Token, cost, model and tool statistics |
| `opencode mcp list` | MCP server status |
| `opencode mcp auth <name>` / `mcp logout <name>` | Authorize or forget a remote MCP server with OAuth |
| `opencode pair [--url U]` | Create a single-use pairing link for a running server (`/auth/connect/:code`) |
| `opencode debug paths\|config\|agents\|skills\|prompt\|plugins` | Troubleshooting (`plugins` also lists plugin tools) |
| `opencode debug plugin-tool <name> [json]` | Execute a plugin tool with the given input |
| `opencode debug plugin-tui <name> [input]` | Execute a plugin TUI command |

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
- **Network fingerprint** (`src/http.rs`): all HTTP goes through BoringSSL configured like
  Bun 1.4.4, so the ClientHello (ciphers, groups, key shares, signature algorithms, extension
  order, ALPN `http/1.1`) is the TypeScript client's, and headers are written as Bun's `fetch`
  writes them: sorted, Bun's casing, joined repeats, then `Connection`, `Accept`, `Host`,
  `Accept-Encoding`, `Content-Length`. `NODE_TLS_REJECT_UNAUTHORIZED=0` disables certificate
  checks, as in Bun.
- **Credentials**: env vars from the catalog, `provider.<id>.options.apiKey`,
  `auth.json`, and API keys saved by the TypeScript client in `opencode.db`
  (read-only).
- **Config**: `opencode.json`/`.jsonc` (global, `OPENCODE_CONFIG`, project chain,
  `.opencode/`, `OPENCODE_CONFIG_CONTENT`), `{env:}`/`{file:}` substitution,
  custom providers/models, `agent`, `command`, `permission`, `mcp`, `tools`,
  `instructions`, `skills`, `lsp`, `compaction`, `formatter`,
  `experimental.todo`, `experimental.schedules` and `experimental.codemode`.
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
  embedded context and images, client MCP servers (stdio and HTTP), usage updates.
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
    and slash commands, key bindings, toasts, dialogs, storage and the plugin
    client. Slot/JSX rendering is not ported, so plugin panels stay inert.
- **TUI** (ratatui): markdown rendering, thinking blocks, tool calls with diffs,
  permission dialog, model/session/agent pickers, command palette, slash
  commands (`/new /sessions /models /agents /compact /variant /thinking /details
  /export /init /review /help /quit` + custom), `@file` attachments, prompt
  history, queued messages.

Sessions are stored as JSON in `~/.local/share/opencode/storage/session/<project>/`.

## Not ported

Things in the original that depend on its hosted services or on domains this
port does not implement: `service`, `github`, `upgrade`, and Claude Pro/Max
OAuth (ChatGPT and GitHub Copilot device login are available through
`opencode auth login`). Bedrock/Vertex/Azure need an OpenAI-compatible
`baseURL`. Shell permission parsing uses a quote-aware splitter instead of
tree-sitter. Code Mode uses Deno with a `tools.*` bridge rather than the
upstream bytecode VM; snapshots, the `search` helper and static tool
authorization are not part of it.

Artifacts, snapshots/undo, post-edit formatters, VCS/worktree strategies and
RPC endpoints are available through the API and plugin host. `opencode serve`
covers sessions (prompt, command, shell, fork, revert, compact, diff, forms,
inbox, permissions, environment, move, generate), messages, agents, providers,
models, projects, plugins, MCP (including add/connect/disconnect), filesystem
read/list/find/write, background shells and jobs, scheduled tasks, web search,
image/decision generation, artifacts, PTY and persistent terminals, VCS and
worktrees, durable session logs over SSE, and pairing/auth. Integrations and
credentials are handled by plugins; `aisdk` SDK/language hooks pass options
through, but a plugin-returned model object cannot replace the Rust provider.

## Tests

`cargo test` covers wildcard/permission evaluation, JSONC and frontmatter
parsing, edit matching (CRLF, typographic, trailing whitespace), patch parsing,
shell command splitting, DB credential import, upstream ID formats, ACP prompt
conversion and diff reconstruction, input editing and TUI rendering.
