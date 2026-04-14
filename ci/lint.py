from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from ci.dev_build import repo_python
from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_COMMAND_TIMEOUT_SECONDS = 10.0
COMMAND_TIMEOUT_ENV = "RUNNING_PROCESS_LINT_COMMAND_TIMEOUT_SECONDS"


def run(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def command_timeout_seconds() -> float | None:
    configured = os.environ.get(COMMAND_TIMEOUT_ENV)
    if configured is None:
        return DEFAULT_COMMAND_TIMEOUT_SECONDS
    configured = configured.strip()
    if not configured:
        return None
    timeout = float(configured)
    if timeout <= 0:
        return None
    return timeout


def supervised_command(python: Path, *command: str) -> list[str]:
    timeout = command_timeout_seconds()
    if timeout is None:
        return list(command)
    return [
        str(python),
        "-m",
        "running_process.cli",
        "--timeout",
        str(timeout),
        "--",
        *command,
    ]


def main() -> int:
    activate, _ = load_env_helpers()
    activate()
    python = repo_python()
    if run(supervised_command(python, str(python), "-m", "ci.version_check")) != 0:
        return 1
    if run(supervised_command(python, str(python), "-m", "ci.spawn_path_guard")) != 0:
        return 1
    if run(supervised_command(python, *cargo_command("fmt", "--all"))) != 0:
        return 1
    if run(
        supervised_command(
            python,
            *cargo_command(
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ),
        )
    ) != 0:
        return 1
    if run(
        supervised_command(
            python,
            str(python),
            "-m",
            "ruff",
            "check",
            "--fix",
            "src",
            "tests",
            "ci",
        )
    ) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
