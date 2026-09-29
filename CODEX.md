# Codex Agent Documentation

First rule: direct Cargo build/check/test/package/publish commands in this repo must be prefixed with `soldr` (the globally installed binary), or you should use the higher-level repo entrypoints that already choose the compatible path for you (`uv run build.py`, `./install`, `./lint`, `./test`). Do not run raw `cargo build`, `cargo check`, `cargo test`, `cargo package`, `rustc`, or `rustfmt` directly.

Read [CLAUDE.md](C:\Users\niteris\dev\running-process\CLAUDE.md) for the rest of the agent documentation and repository guidance.
