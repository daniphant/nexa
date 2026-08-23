# Nexa

A small coding-agent harness in Rust.

Nexa currently ships one native harness implementation: an OpenAI-compatible
provider adapter, agent loop, workspace tools, authoritative session runtime,
server, and CLI client. Clients choose a provider and model for each message;
the runtime component records the user message, streamed response, tool activity,
and terminal run state in one durable ordered event stream.

The native harness is part of Nexa rather than a replaceable plugin. Swapping in
Pi or another harness implementation is not supported in the current slice.

## Workspace

- `apps/cli` owns provider setup and the `nexa` command entry point.
- `apps/desktop` is the native GPU-rendered desktop client (egui + wgpu).
- `apps/server` owns the local HTTP/SSE process.
- `crates/client` is the typed Rust client for commands, provider discovery, and events.
- `crates/protocol` defines commands, events, model references, and tool wire types.
- `crates/runtime` owns the authoritative session and durable event stream.
- `crates/harness` owns the provider adapter, inference loop, and tool bridge.
- `crates/tui` owns the fullscreen terminal UI and its local presentation state.

Inference and tools run outside each session actor. Their normalized events are
sent back to the actor before being appended to that session's event log and
broadcast to clients. A server-owned session registry binds each default session
to one canonical workspace, so the background server is independent of its own
working directory.

## Nexa home

Nexa keeps user-level state under `~/.nexa` by default:

- `provider.toml` contains provider names, endpoints, API formats, and models.
- `credentials.toml` contains API keys and is written with `0600` permissions on Unix.
- `server.token` authenticates clients to the local server and is written with
  `0600` permissions on Unix.
- `settings.toml` stores client defaults such as the last selected model and
  reasoning effort.
- `sessions/<session-id>/session.json` stores each session's workspace binding
  and activity timestamps. Every session gets a random ID, so workspaces may
  hold any number of sessions.
- `sessions/<session-id>/events.ndjson` is that session's durable event stream.
Set `NEXA_HOME` to move the complete state directory. The more specific
`NEXA_PROVIDER_FILE`, `NEXA_CREDENTIALS_FILE`, `NEXA_SERVER_TOKEN_FILE`, and
`NEXA_SESSIONS_DIR` overrides are useful for isolated development and tests.

## Provider registry

The CLI can add one OpenAI-compatible Chat Completions provider interactively:

```sh
cargo run -p nexa-cli -- provider add
```

Running `nexa` with no configured providers starts the same setup automatically.

It asks for the provider name, base URL, and API key, then lists the available
models by querying `GET {base_url}/models`. Models are chosen from that list in
a checkbox picker: arrow keys move, spacebar checks individual models, and enter
continues once at least one model is checked. The API key is entered without
terminal echo and never written to `provider.toml`. When a provider does not
implement model listing, setup falls back to typing model IDs manually,
separated by commas.

Credentials are sent as `Authorization: Bearer` by default. If discovery is
rejected that way but succeeds with an `x-api-key` header (Anthropic-style
gateways), setup saves `auth = "x-api-key"` for the provider and every later
request uses that header instead.

## Agent presets

Agent presets are named tool scopes, modeled after DeepSeek Harness's
agent presets. Each preset is a TOML file in
`$NEXA_HOME/agent-presets/` (`NEXA_PRESETS_DIR` overrides):

```toml
# ~/.nexa/agent-presets/read-only.toml
description = "Read-only review agent"
tools = ["read_file"]
```

An empty or missing `tools` list means every tool is available. Presets are
loaded when the server starts; pick one per chat in the desktop app's composer
bar, or send it per message as `"preset": "read-only"` on `/commands`.

Model entries may carry the reasoning effort levels a provider reports for
them. Setup parses capability extensions such as OpenRouter's
`supported_parameters`, LiteLLM's `supported_openai_params`, and boolean flags
like `supports_reasoning` when they are present:

```toml
[providers.local]
name = "Local"
base_url = "http://127.0.0.1:8080/v1"

[[providers.local.models]]
id = "thinking-model"
reasoning_efforts = ["low", "high", "xhigh"]

[[providers.local.models]]
id = "plain-model"
```

The resulting provider registry has this shape:

```toml
[providers.xai]
name = "xAI"
base_url = "https://api.x.ai/v1"
api_format = "chat_completions"
models = ["grok-code-fast-1"]

[providers.local]
name = "Local"
base_url = "http://127.0.0.1:8080/v1"
api_format = "chat_completions"
models = ["grok-code-fast-1", "local-coder"]
```

Model IDs do not need to be globally unique. Each command carries the explicit
provider/model pair, such as `{ "provider": "local", "id":
"grok-code-fast-1" }`.

For hand-written configuration, `api_key_env` remains available as an alternative
to the credential store. Environment credentials take precedence. Providers
without authentication can omit both.

Set `NEXA_WORKSPACE` before starting the CLI to select a workspace other than
the CLI's current directory. The CLI sends that directory when it opens the
workspace's default session; the long-lived server never uses its own current
directory as a tool root.

## Desktop client

`apps/desktop` is a native GPU-rendered client (`nexa-desktop`). Launching
it starts `nexa-server` in the background when the default local server is
not already running, the same way the CLI does. Before a chat starts, the
composer sits centered on the canvas; once a thread exists, it pins to the
bottom. Workspace, agent-preset, model, and
reasoning pickers live on the composer as pills. Each picker is a
popover: it opens above a docked composer (below an empty-state one),
scrolls long lists, and closes on a choice or a click outside.

The reasoning popover lists only the effort levels the current model
accepts — the declared list when the provider enumerates them, otherwise
a family table (GPT-5.6 Sol/Luna/Terra include `max`; unknown models use
`low` / `medium` / `high` / `xhigh`). The picker is hidden when a model
declares an empty list. The model popover groups entries by provider and
filters as you type.

A "project" is a workspace directory. The workspace pill lists recently
used folders and opens the OS's native folder picker to add another;
picking one only changes where the *next* new chat is created, so
switching projects never disturbs an open thread. The sidebar lists
every known workspace's chats as an expandable tree, using the first
prompt as the title once that chat has been opened. Recently used
workspaces, the active one, and the default agent preset are persisted
under `[desktop]` in `~/.nexa/settings.toml`, alongside the `[models]`
block the CLI already writes there.

The composer is multiline: Enter sends, Shift+Enter inserts a newline.
Ctrl/Cmd+N starts a new chat, Ctrl/Cmd+, opens Settings, Ctrl/Cmd+Shift+A
toggles the activity inspector, and Ctrl/Cmd+L focuses the composer.

The Settings modal (bottom of the sidebar) has three tabs: **General**
(default agent preset, default model and reasoning — picking either in
the composer updates these too), **Models** (a read-only view of every
configured provider and its models), and **Agent presets** (a read-only
view of loaded presets, tools included, with the current default
flagged). Adding providers or editing credentials from the desktop app
isn't supported yet — that still requires `nexa provider add`.

The desktop bundles Inter, JetBrains Mono, and Noto Sans Symbols 2 so UI
controls and transcript text retain their glyphs without depending on fonts
installed on the host.

## Install from this checkout

Install the pinned toolchain, then install or update both release binaries in
Cargo's executable directory:

```sh
mise install
cargo install --locked --force --path apps/server
cargo install --locked --force --path apps/cli
cargo install --locked --force --path apps/desktop
```

With Cargo's executable directory on `PATH`, `nexa` and `nexa-server` are then
available from every directory. Re-run the commands after making local changes
that you want reflected in the installed binaries.

## Run the active session

Configure a provider:

```sh
cargo run -p nexa-cli -- provider add
```

Then start the CLI:

```sh
cargo run -p nexa-cli
```

Running `nexa` starts a fresh chat session for the current workspace and opens
a fullscreen terminal UI. It replays that session's durable transcript, streams
assistant text, shows compact tool activity, and — the first time only, when
more than one provider/model pair is available — asks you to choose. That
choice is persisted in `~/.nexa/settings.toml` and reused on every later
launch:

```toml
[models]
default = "local/thinking-model"
default_reasoning_effort = "high"
```

Switching models or efforts in a session updates the saved default on exit.
Reasoning effort is sent to providers as `reasoning_effort`; when a model
declares which levels it supports, unsupported selections are rejected.

The terminal UI uses these controls:

- `Enter` sends the current one-line message.
- `/new` starts a fresh chat for this workspace and switches to it.
- `/sessions` lists every session for this workspace, most recent first;
  pick one with `↑/↓` + `Enter` to switch — its transcript replays in place.
- `/model` opens the model picker; `/model provider/model` switches directly.
- `/reasoning` cycles effort levels; `/reasoning high` selects one directly.
- `/clear` clears the transcript view (the durable log is kept).
- `/quit`, `/exit` leave Nexa.
- `F2` also opens the provider/model picker.
- `Page Up`, `Page Down`, arrow keys, or the mouse wheel scroll the transcript.
- `Ctrl+U` clears the composer.
- `Ctrl+C` leaves Nexa.

When the default local server is not running, the CLI and the desktop
client start `nexa-server` as a detached background process and wait for
it to become ready. Server output is appended to `$NEXA_HOME/logs/server.log`,
which defaults to `~/.nexa/logs/server.log`.

An explicit `NEXA_SERVER_URL` remains externally managed and is never replaced
by an automatically started local server. Set `NEXA_SERVER_TOKEN` when that
server requires bearer authentication.

The HTTP protocol remains available to every future client. Local clients read
the owner-only server token and send it as a bearer token. To open the default
session for a workspace manually:

```sh
token_file="${NEXA_SERVER_TOKEN_FILE:-${NEXA_HOME:-$HOME/.nexa}/server.token}"
read -r nexa_server_token < "$token_file"
curl -X POST http://127.0.0.1:4123/sessions/create \
  -H "authorization: Bearer $nexa_server_token" \
  -H 'content-type: application/json' \
  -d '{"workspace":"/absolute/path/to/workspace"}'
unset nexa_server_token token_file
```

The response contains a stable session ID. Its replayable event stream is
`GET /sessions/<session-id>/events`, and commands sent to `POST /commands`
include that `sessionId`.

On first use after upgrading from the original singleton runtime, Nexa assigns
the legacy `sessions/local.ndjson` history to the first workspace opened and
keeps the original as `sessions/local.ndjson.migrated`. A malformed legacy log
is moved to `sessions/local.ndjson.corrupt` instead of blocking every workspace.

The harness currently exposes exactly two workspace-scoped tools:

- `read_file` reads a bounded UTF-8 text file.
- `edit_file` creates a missing file when `old_text` is empty, or replaces one
  exact, unique text fragment and reports before/after hashes.

Absolute paths, parent traversal, symlink escapes, binary reads, oversized reads,
implicit overwrites, zero-match edits, and ambiguous edits are rejected.

## Verification

```sh
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

This slice intentionally does not include a session browser, multiple named
sessions per workspace, queue UI, steering, orchestration, a native app, OAuth,
Anthropic Messages, OpenAI Responses, replaceable harness implementations, or a
broader coding-tool suite.
