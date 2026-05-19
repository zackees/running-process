from __future__ import annotations

from ci.codex_hooks import pre_tool_use_response


def test_codex_pre_tool_use_blocks_direct_raw_build_command() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build --workspace",
            },
        }
    )

    assert response == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": (
                "This repo requires build-related shell commands to go through `soldr`. "
                "Run `soldr cargo build --workspace` instead."
            ),
        }
    }


def test_codex_pre_tool_use_allows_soldr_command() -> None:
    assert pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "soldr cargo test --workspace",
            },
        }
    ) is None


def test_codex_pre_tool_use_blocks_compound_raw_build_command() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build && cargo test",
            },
        }
    )

    assert response == {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": (
                    "Build-related shell commands in this repo must run through `soldr` "
                    "or the higher-level repo entrypoints "
                    "(`uv run build.py`, `./install`, `./lint`, `./test`)."
                ),
            }
        }
