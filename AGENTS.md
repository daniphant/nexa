# AGENTS.md

Instructions for coding agents working in this repository.

## Project

Nexa is a small coding-agent harness in Rust: one Cargo workspace containing the
`nexa` CLI (`apps/cli`), the `nexa-server` HTTP/SSE process (`apps/server`), and
the shared crates under `crates/`. See `README.md` and `docs/` for architecture
and product context before making changes.

## Build and verify

The pinned toolchain is declared in `mise.toml`; run `mise install` first if the
toolchain is missing. Always run all three checks after changes and make them
pass before finishing:

```sh
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

CI-equivalent strictness applies: warnings are errors (`-D warnings`), unsafe
code is forbidden workspace-wide, and dependency versions are exact-pinned with
`=` in `Cargo.toml`. Keep that style when adding dependencies.

## Install updated binaries on this machine

Testing is done by running `nexa` directly on this box, so compilation and
exposure of the binaries is an expected step after every change — do not stop
at `cargo build`. Build in release mode and reinstall all three binaries
(`nexa`, `nexa-server`, and the desktop client) into Cargo's executable
directory:

```sh
cargo install --locked --force --path apps/server
cargo install --locked --force --path apps/cli
cargo install --locked --force --path apps/desktop
```

This overwrites the installed `~/.cargo/bin/nexa-server`, `~/.cargo/bin/nexa`,
and `~/.cargo/bin/nexa-desktop`.
Re-run both commands whenever source changes should be reflected in what the
user invokes; installing only `nexa` leaves a stale `nexa-server`, which the CLI
auto-starts, so always refresh the pair together.

On this machine a second copy of both binaries lives in `~/.local/bin` and may
shadow the Cargo install depending on the user's shell PATH. After installing,
sync that copy too and verify which binary the user actually resolves:

```sh
cp -f target/release/nexa ~/.local/bin/nexa
cp -f target/release/nexa-server ~/.local/bin/nexa-server
cp -f target/release/nexa-desktop ~/.local/bin/nexa-desktop
which -a nexa
```

For quick iteration, `cargo run -p nexa-cli -- <args>` works without installing,
but it is not a substitute for the release install above.

## Conventions

- Match the surrounding code style; the codebase avoids non-standard shortcuts.
- State lives under `~/.nexa` (override with `NEXA_HOME`); credentials must
  never be written to `provider.toml` — only to `credentials.toml` (`0600`).
- Interactive terminal features belong in `apps/cli` or `crates/tui`; provider
  and inference logic belongs in `crates/harness`.
- Update `README.md` when user-visible behavior changes.
