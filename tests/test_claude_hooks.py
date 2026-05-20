from __future__ import annotations

from ci.claude_hooks import MANDATE_REASON, evaluate_bash_command, pre_tool_use_response


def test_evaluate_bash_command_denies_direct_cargo() -> None:
    decision = evaluate_bash_command("cargo build --workspace")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == MANDATE_REASON


def test_evaluate_bash_command_ignores_direct_maturin() -> None:
    assert evaluate_bash_command("maturin build --release") is None


def test_evaluate_bash_command_allows_soldr_prefix() -> None:
    assert evaluate_bash_command("soldr cargo test --workspace") is None


def test_evaluate_bash_command_denies_compound_raw_build_command() -> None:
    decision = evaluate_bash_command("cargo build && cargo test")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == MANDATE_REASON


def test_evaluate_bash_command_denies_raw_rustc() -> None:
    decision = evaluate_bash_command("rustc --version")

    assert decision is not None
    assert decision.permission_decision == "deny"


def test_evaluate_bash_command_allows_repo_entrypoints() -> None:
    assert evaluate_bash_command("./test") is None
    assert evaluate_bash_command("./lint") is None
    assert evaluate_bash_command("uv run build.py") is None


def test_evaluate_bash_command_does_not_match_cargo_inside_other_args() -> None:
    assert evaluate_bash_command('gh pr create --body "cargo build --workspace"') is None


def test_pre_tool_use_response_denies_raw_cargo() -> None:
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
            "permissionDecision": "deny",
            "permissionDecisionReason": MANDATE_REASON,
        }
    }
