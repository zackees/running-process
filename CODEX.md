# Codex Agent Documentation

First rule: all Rust build-chain commands in this repo must go through `uvx soldr` or the higher-level repo entrypoints that already do so for you (`uv run build.py`, `./install`, `./lint`, `./test`). Do not run raw `cargo`, `rustc`, `rustfmt`, or `maturin` build commands directly.

Read [CLAUDE.md](C:\Users\niteris\dev\running-process\CLAUDE.md) for the rest of the agent documentation and repository guidance.

Codex project config lives in [.codex/config.toml](C:\Users\niteris\dev\running-process\.codex\config.toml) and [.codex/hooks.json](C:\Users\niteris\dev\running-process\.codex\hooks.json). The checked-in Codex `PreToolUse` hook blocks raw build-tool Bash commands unless they already go through `uvx soldr` or the higher-level repo entrypoints.

Codex execpolicy rules also live in [.codex/rules/soldr.rules](C:\Users\niteris\dev\running-process\.codex\rules\soldr.rules) and forbid raw `cargo`, `maturin`, `rustc`, and `rustfmt` commands.
