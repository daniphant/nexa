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
- `sessions/<session-id>/session.json` stores the immutable workspace binding.
- `sessions/<session-id>/events.ndjson` is that session's durable event stream.

Set `NEXA_HOME` to move the complete state directory. The more specific
`NEXA_PROVIDER_FILE`, `NEXA_CREDENTIALS_FILE`, and `NEXA_SESSIONS_DIR` overrides are
useful for isolated development and tests.

## Provider registry

The CLI can add one OpenAI-compatible Chat Completions provider interactively:

```sh
cargo run -p nexa-cli -- provider add
```

Running `nexa` with no configured providers starts the same setup automatically.

It asks for the provider name, base URL, initial model ID, and API key. The key
is entered without terminal echo and never written to `provider.toml`.

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

## Install from this checkout

Install or update both release binaries in Cargo's executable directory:

```sh
cargo install --locked --force --path apps/server
cargo install --locked --force --path apps/cli
```

With Cargo's executable directory on `PATH`, `nexa` and `nexa-server` are then
available from every directory. Re-run the commands after making local changes
that you want reflected in the installed binaries.

## Run the active session

Install the exact Rust toolchain pinned for the project:

```sh
mise install
```

Configure a provider:

```sh
cargo run -p nexa-cli -- provider add
```

Then start the CLI:

```sh
cargo run -p nexa-cli
```

Running `nexa` opens a fullscreen terminal UI for the current workspace's
default session. It replays the durable transcript, streams assistant text,
shows compact tool activity, and asks you to choose when more than one
provider/model pair is available. When the default local server is not running, the CLI starts
`nexa-server` as a detached background process and waits for it to become ready.
Server output is appended to `~/.nexa/logs/server.log`.

An explicit `NEXA_SERVER_URL` remains externally managed and is never replaced
by an automatically started local server.

The terminal UI uses these controls:

- `Enter` sends the current one-line message.
- `Ctrl+M` opens the provider/model picker.
- `Page Up`, `Page Down`, arrow keys, or the mouse wheel scroll the transcript.
- `Ctrl+U` clears the composer.
- `Ctrl+C`, `/quit`, or `/exit` leaves Nexa.

The HTTP protocol remains available to every future client. First open the
default session for a workspace:

```sh
curl -X POST http://127.0.0.1:4123/sessions/open \
  -H 'content-type: application/json' \
  -d '{"workspace":"/absolute/path/to/workspace"}'
```

The response contains a stable session ID. Its replayable event stream is
`GET /sessions/<session-id>/events`, and commands sent to `POST /commands`
include that `sessionId`.

On first use after upgrading from the original singleton runtime, Nexa assigns
the legacy `sessions/local.ndjson` history to the first workspace opened and
keeps the original as `sessions/local.ndjson.migrated`.

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
