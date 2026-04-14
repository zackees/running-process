from __future__ import annotations

from dataclasses import dataclass

BUILD_TOOL_PREFIXES = (
    "rustc",
    "rustfmt",
    "clippy-driver",
)
SUPPORTED_CARGO_SUBCOMMANDS = ("build", "check", "test", "package", "publish")
ALLOW_PREFIXES = (
    "uvx soldr ",
    "uvx soldr.exe ",
    "uv run build.py",
    "uv run --module ci.build_wheel",
    "uv run -m ci.build_wheel",
    "python build.py",
    "python -m ci.build_wheel",
    "./install",
    ".\\install",
    "./lint",
    ".\\lint",
    "./test",
    ".\\test",
)
SHELL_SEPARATORS = ("&&", "||", "|", ";", "\n")


@dataclass(frozen=True)
class HookDecision:
    permission_decision: str
    reason: str
    updated_command: str | None = None


def _starts_with_any(command: str, prefixes: tuple[str, ...]) -> bool:
    lowered = command.lstrip().lower()
    return any(lowered.startswith(prefix) for prefix in prefixes)


def _contains_raw_build_tool(command: str) -> bool:
    lowered = command.lower()
    for subcommand in SUPPORTED_CARGO_SUBCOMMANDS:
        if lowered.startswith(f"cargo {subcommand} "):
            return True
        if lowered == f"cargo {subcommand}":
            return True
        if any(f"{sep} cargo {subcommand} " in lowered for sep in ("&&", "||", ";", "|")):
            return True
        if f"\ncargo {subcommand} " in lowered:
            return True
    for tool in BUILD_TOOL_PREFIXES:
        if lowered.startswith(f"{tool} ") or lowered == tool:
            return True
        if any(f"{sep} {tool} " in lowered for sep in ("&&", "||", ";", "|")):
            return True
        if f"\n{tool} " in lowered:
            return True
    return False


def _rewrite_direct_command(command: str) -> str | None:
    stripped = command.lstrip()
    leading = command[: len(command) - len(stripped)]
    for subcommand in SUPPORTED_CARGO_SUBCOMMANDS:
        if stripped == f"cargo {subcommand}" or stripped.startswith(f"cargo {subcommand} "):
            return f"{leading}uvx soldr {stripped}"
    for tool in BUILD_TOOL_PREFIXES:
        if stripped == tool or stripped.startswith(f"{tool} "):
            return f"{leading}uvx soldr {stripped}"
    return None


def evaluate_bash_command(command: str) -> HookDecision | None:
    if not command.strip():
        return None
    if _starts_with_any(command, ALLOW_PREFIXES):
        return None
    if _contains_raw_build_tool(command) and any(
        separator in command for separator in SHELL_SEPARATORS
    ):
        return HookDecision(
            permission_decision="deny",
            reason=(
                "Build-related shell commands in this repo must run through `uvx soldr` "
                "or the higher-level repo entrypoints "
                "(`uv run build.py`, `./install`, `./lint`, `./test`)."
            ),
        )
    rewritten = _rewrite_direct_command(command)
    if rewritten is not None:
        return HookDecision(
            permission_decision="allow",
            reason="Rewriting build command through uvx soldr",
            updated_command=rewritten,
        )
    return None


def pre_tool_use_response(payload: dict[str, object]) -> dict[str, object] | None:
    tool_name = payload.get("tool_name")
    if tool_name != "Bash":
        return None
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return None
    command = tool_input.get("command")
    if not isinstance(command, str):
        return None
    decision = evaluate_bash_command(command)
    if decision is None:
        return None

    hook_output: dict[str, object] = {
        "hookEventName": "PreToolUse",
        "permissionDecision": decision.permission_decision,
        "permissionDecisionReason": decision.reason,
    }
    if decision.updated_command is not None:
        updated_input = dict(tool_input)
        updated_input["command"] = decision.updated_command
        hook_output["updatedInput"] = updated_input
    return {"hookSpecificOutput": hook_output}
