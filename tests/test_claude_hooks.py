from __future__ import annotations

from ci.claude_hooks import evaluate_bash_command, pre_tool_use_response


def test_evaluate_bash_command_rewrites_direct_cargo() -> None:
    decision = evaluate_bash_command("cargo build --workspace")

    assert decision is not None
    assert decision.permission_decision == "allow"
    assert decision.updated_command == "uvx soldr cargo build --workspace"


def test_evaluate_bash_command_rewrites_direct_maturin() -> None:
    assert evaluate_bash_command("maturin build --release") is None


def test_evaluate_bash_command_allows_uvx_soldr() -> None:
    assert evaluate_bash_command("uvx soldr cargo test --workspace") is None


def test_evaluate_bash_command_blocks_compound_raw_build_command() -> None:
    decision = evaluate_bash_command("cargo build && cargo test")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.updated_command is None


def test_evaluate_bash_command_does_not_match_cargo_inside_other_args() -> None:
    assert evaluate_bash_command('gh pr create --body "cargo build --workspace"') is None


def test_pre_tool_use_response_rewrites_bash_input() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build --workspace",
                "description": "build rust",
            },
        }
    )

    assert response == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": "Rewriting build command through uvx soldr",
            "updatedInput": {
                "command": "uvx soldr cargo build --workspace",
                "description": "build rust",
            },
        }
    }
