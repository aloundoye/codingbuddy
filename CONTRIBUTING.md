# Contributing to CodingBuddy

Thanks for helping. This guide covers what you need to get a change merged.
Architecture notes for contributors and coding agents live in
[AGENTS.md](AGENTS.md).

## Setup

- Rust toolchain from `rust-toolchain.toml` (installed automatically by `rustup`)
- Linux: `sudo apt-get install -y bubblewrap libdbus-1-dev pkg-config`
- Node 22 only if you touch `extensions/vscode` or `web/`

Rust build output is large (tens of GB). Keep the checkout, or at least
`CARGO_TARGET_DIR`, on a fast internal disk.

## Before you open a pull request

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

On Linux, CI also runs `scripts/parity_regression_check.sh` and
`scripts/runtime_conformance_scan.sh`.

## Pull requests

- Branch from `dev`; keep one topic per pull request.
- Add a test that fails without your change. Tool-loop behaviour is tested in
  `crates/codingbuddy-agent/tests/` with a scripted LLM (see AGENTS.md).
- Add a line under **Unreleased** in [CHANGELOG.md](CHANGELOG.md) for any
  user-visible change.
- If you add a `StreamChunk` variant, a hook event, a crate or a command,
  update AGENTS.md in the same pull request.
- Never commit API keys, `.env` files or `settings.local.json`.

## Reporting bugs

Open an issue with your version (`codingbuddy --version`), OS, provider and
model, and the steps to reproduce. For security problems, follow
[SECURITY.md](SECURITY.md) instead.
