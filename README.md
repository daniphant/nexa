# Nexa

A lightweight, headless runtime for coordinated agent sessions.

The current slice runs one local agent session through an OpenAI-compatible
provider registry. Clients choose a provider and model for each message; the
runtime records the user message, streamed response, tool activity, and terminal
run state in one durable ordered event stream.

## Workspace

- `apps/server` owns the local HTTP/SSE process.
- `crates/protocol` defines commands, events, model references, and tool wire types.
- `crates/runtime` owns the authoritative session and durable event stream.
- `crates/harness` owns the provider adapter, inference loop, and tool bridge.

Inference and tools run outside the session actor. Their normalized events are
sent back to the actor before being appended to `data/local.ndjson` and broadcast
to clients.

## Provider registry

Copy `provider.example.toml` to `provider.toml` and list any
OpenAI-compatible endpoints you want Nexa to route to:

```toml
[providers.xai]
base_url = "https://api.x.ai/v1"
models = ["grok-code-fast-1"]
api_key_env = "XAI_API_KEY"

[providers.local]
base_url = "http://127.0.0.1:8080/v1"
models = ["grok-code-fast-1", "local-coder"]
```

Model IDs do not need to be globally unique. Each command carries the explicit
provider/model pair, such as `{ "provider": "local", "id":
"grok-code-fast-1" }`.

API keys are never stored in `provider.toml`. When `api_key_env` is present,
Nexa reads that environment variable when a run selects the provider. Providers
without authentication can omit it.

Set `NEXA_PROVIDER_FILE` to load the registry from another path and
`NEXA_WORKSPACE` to change the directory exposed to tools.

## Run the active session

Install the exact Rust toolchain pinned for the project:

```sh
mise install
```

Start the runtime:

```sh
cp provider.example.toml provider.toml
mise exec -- cargo run -p nexa-server
```

Watch the replayable event stream:

```sh
curl -N http://127.0.0.1:4123/events
```

Send a message using one configured provider/model pair:

```sh
curl -X POST http://127.0.0.1:4123/commands \
  -H 'content-type: application/json' \
  -d '{
    "type": "send_message",
    "clientId": "alice",
    "model": { "provider": "local", "id": "local-coder" },
    "text": "Read README.md and tell me what Nexa does."
  }'
```

The harness currently exposes exactly two workspace-scoped tools:

- `read_file` reads a bounded UTF-8 text file.
- `edit_file` replaces one exact, unique text fragment and reports before/after hashes.

Absolute paths, parent traversal, symlink escapes, binary reads, oversized reads,
zero-match edits, and ambiguous edits are rejected.

## Verification

```sh
mise exec -- cargo test --workspace
mise exec -- cargo fmt --all --check
mise exec -- cargo clippy --workspace --all-targets --all-features -- -D warnings
```

This slice intentionally does not include a CLI/TUI/native UI, multiple sessions,
queued messages, steering, orchestration, Anthropic or OpenAI-native protocols,
credential persistence, or a broader coding-tool suite.
