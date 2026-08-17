# Nexa

A lightweight, headless runtime for coordinated agent sessions.

The first milestone proves that two local clients can share one authoritative,
replayable session stream.

## First milestone

The current slice contains only a Rust coordination server and the two crates it
directly needs:

- `apps/server` owns the local HTTP/SSE process.
- `crates/protocol` defines this slice's commands and events.
- `crates/runtime` owns the authoritative session and durable event stream.

- `POST /commands` accepts a message for the single local session.
- `GET /events` replays that session's history, then streams new events with
  server-sent events.
- The runtime assigns each event's sequence, appends it once to
  `data/local.ndjson`, and broadcasts it to every connected client.

The HTTP/SSE protocol is language-neutral. Product UI is intentionally outside
this milestone. Future agent harnesses will run behind adapters or process
boundaries so their work does not execute inside the session actor.

Install the exact Rust toolchain pinned for this project:

```sh
mise install
```

Start the runtime:

```sh
mise exec -- cargo run -p nexa-server
```

Portless is an optional development wrapper. It supplies the `PORT` environment
variable that the server already understands and gives each worktree a stable
local URL:

```sh
portless run --name nexa mise exec -- cargo run -p nexa-server
```

Connect two event streams in separate terminals:

```sh
curl -N http://127.0.0.1:4123/events
```

Send a message from either client identity:

```sh
curl -X POST http://127.0.0.1:4123/commands \
  -H 'content-type: application/json' \
  -d '{"type":"send_message","clientId":"alice","text":"hello"}'
```

Run the focused integration test:

```sh
mise exec -- cargo test --workspace
```

## Code quality

Nexa uses Rustfmt for formatting and Clippy for linting. The Rust toolchain and
every direct crate dependency are pinned exactly.

```sh
mise exec -- cargo fmt --all --check
mise exec -- cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Not included yet: agent execution or SDKs, multiple sessions, authentication,
remote networking, multiple-user identity, routing, orchestration, human review,
resource lifecycle, or any product UI.
