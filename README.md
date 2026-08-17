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

- `apps/cli` owns provider setup and the interactive terminal client.
- `apps/server` owns the local HTTP/SSE process.
- `crates/protocol` defines commands, events, model references, and tool wire types.
- `crates/runtime` owns the authoritative session and durable event stream.
- `crates/harness` owns the provider adapter, inference loop, and tool bridge.

Inference and tools run outside the session actor. Their normalized events are
sent back to the actor before being appended to `~/.nexa/sessions/local.ndjson`
and broadcast to clients.

## Nexa home

Nexa keeps user-level state under `~/.nexa` by default:

- `provider.toml` contains provider names, endpoints, API formats, and models.
- `credentials.toml` contains API keys and is written with `0600` permissions on Unix.
- `sessions/local.ndjson` is the durable event stream for the current local session.

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

Set `NEXA_WORKSPACE` to change the directory exposed to tools.

## Run the active session

Install the exact Rust toolchain pinned for the project:

```sh
mise install
```

Configure a provider, then start the Nexa server:

```sh
cargo run -p nexa-cli -- provider add
cargo run -p nexa-server
```

In another terminal, start the CLI:

```sh
cargo run -p nexa-cli
```

The CLI obtains the provider/model catalog from the server, asks you to choose
when more than one model is available, and streams each response. Type `/quit`
to leave.

The HTTP protocol remains available to every future client. Watch the replayable
event stream with:

```sh
curl -N http://127.0.0.1:4123/events
```

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

This slice intentionally does not include a TUI/native UI, multiple sessions,
queued messages, steering, orchestration, OAuth, Anthropic Messages, OpenAI
Responses, replaceable harness implementations, or a broader coding-tool suite.
