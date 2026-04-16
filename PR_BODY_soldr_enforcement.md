## Summary
- route Rust build-chain entrypoints through `uvx soldr`
- retire the `_cargo` / `_rustc` / `_rustfmt` wrappers
- add Claude and Codex project policy files plus Codex execpolicy rules for blocking raw build commands

## Validation
- `uv run python -m running_process.cli --timeout 120 -- uv run pytest tests/test_ci_lint.py tests/test_ci_test.py tests/test_ci_soldr.py tests/test_claude_hooks.py tests/test_codex_hooks.py -q`
- `codex execpolicy check --pretty --rules .codex/rules/soldr.rules -- cargo build --workspace`
- `codex execpolicy check --pretty --rules .codex/rules/soldr.rules -- python -m maturin build --release`
- `codex execpolicy check --pretty --rules .codex/rules/soldr.rules -- uvx soldr cargo build --workspace`
- `codex execpolicy check --pretty --rules .codex/rules/soldr.rules -- uvx soldr maturin build --release`
