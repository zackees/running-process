from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from ci.dev_build import ensure_dev_wheel, repo_python

ROOT = Path(__file__).resolve().parent.parent
IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"
DEFAULT_TEST_TIMEOUT_SECONDS = "10"
DEFAULT_COMMAND_TIMEOUT_SECONDS = 10.0
DEFAULT_LINUX_TEST_TIMEOUT_SECONDS = 180.0
COMMAND_TIMEOUT_ENV = "RUNNING_PROCESS_TEST_COMMAND_TIMEOUT_SECONDS"


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


def supervised_command(
    python: Path,
    *command: str,
    timeout: float | None = None,
) -> list[str]:
    effective_timeout = command_timeout_seconds() if timeout is None else timeout
    if effective_timeout is None:
        return list(command)
    return [
        str(python),
        "-m",
        "running_process.cli",
        "--timeout",
        str(effective_timeout),
        "--",
        *command,
    ]


def _supervised_pytest_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    return supervised_command(python, str(python), "-m", "pytest", *pytest_args)


def _linux_unit_test_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    return supervised_command(
        python,
        str(python),
        str(ROOT / "run_linux_tests.py"),
        *pytest_args,
        timeout=DEFAULT_LINUX_TEST_TIMEOUT_SECONDS,
    )


def run(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def run_live(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    env = clean_env()
    env["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    return subprocess.run(cmd, cwd=ROOT, env=env).returncode


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def parse_args(argv: list[str] | None = None) -> list[str]:
    argv = list(sys.argv[1:] if argv is None else argv)
    pytest_args: list[str] = []
    while argv:
        current = argv.pop(0)
        if current == "--no-skip":
            os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] = "1"
            continue
        pytest_args.append(current)
    return pytest_args


def main(argv: list[str] | None = None) -> int:
    pytest_args = parse_args(argv)
    activate, _ = load_env_helpers()
    activate()
    os.environ.setdefault("RUNNING_PROCESS_TEST_TIMEOUT_SECONDS", DEFAULT_TEST_TIMEOUT_SECONDS)
    python = repo_python()
    if os.environ.get(IN_RUNNING_PROCESS_ENV) != IN_RUNNING_PROCESS_VALUE:
        try:
            ensure_dev_wheel(python, root=ROOT)
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1
    if run(supervised_command(python, "cargo", "test", "--workspace")) != 0:
        return 1
    if run(_supervised_pytest_command(python, "-m", "not live", *pytest_args)) != 0:
        return 1
    if run(_linux_unit_test_command(python, *pytest_args)) != 0:
        return 1
    if run_live(_supervised_pytest_command(python, "-m", "live", *pytest_args)) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
