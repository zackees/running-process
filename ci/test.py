from __future__ import annotations

import os
import shlex
import subprocess
import sys
from pathlib import Path

from ci.dev_build import ensure_dev_wheel

ROOT = Path(__file__).resolve().parent.parent
IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"
GITHUB_ACTIONS_ENV = "GITHUB_ACTIONS"
SKIP_LINUX_DOCKER_ENV = "RUNNING_PROCESS_SKIP_LINUX_DOCKER"
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
    command = [
        str(python),
        "-m",
        "ci.linux_docker",
        "all",
        "--output-dir",
        str(ROOT / "linux"),
    ]
    if pytest_args:
        command.extend(["--pytest-args", shlex.join(pytest_args)])
    return supervised_command(
        python,
        *command,
        timeout=DEFAULT_LINUX_TEST_TIMEOUT_SECONDS,
    )


def running_on_github_actions() -> bool:
    return os.environ.get(GITHUB_ACTIONS_ENV, "").lower() == "true"


def skip_linux_docker_preflight() -> bool:
    return os.environ.get(SKIP_LINUX_DOCKER_ENV, "").lower() in {"1", "true", "yes", "on"}


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


def _looks_like_pytest_target(arg: str) -> bool:
    return (
        arg.endswith(".py")
        or "::" in arg
        or "/" in arg
        or "\\" in arg
    )


def _normalize_pytest_args(args: list[str]) -> list[str]:
    if not args:
        return []
    if any(arg.startswith("-") for arg in args):
        return list(args)
    targets: list[str] = []
    selectors: list[str] = []
    collecting_targets = True
    for arg in args:
        if collecting_targets and _looks_like_pytest_target(arg):
            targets.append(arg)
            continue
        collecting_targets = False
        selectors.append(arg)
    normalized = list(targets or args[:1])
    if selectors:
        normalized.extend(["-k", " and ".join(selectors)])
    return normalized


def parse_args(argv: list[str] | None = None) -> tuple[list[str], bool]:
    argv = list(sys.argv[1:] if argv is None else argv)
    raw_pytest_args: list[str] = []
    require_symbols = False
    while argv:
        current = argv.pop(0)
        if current == "--no-skip":
            require_symbols = True
            continue
        raw_pytest_args.append(current)
    return _normalize_pytest_args(raw_pytest_args), require_symbols


def main(argv: list[str] | None = None) -> int:
    pytest_args, require_symbols = parse_args(argv)
    activate, _ = load_env_helpers()
    activate()
    if require_symbols:
        os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] = "1"
    os.environ.setdefault("RUNNING_PROCESS_TEST_TIMEOUT_SECONDS", DEFAULT_TEST_TIMEOUT_SECONDS)
    python = Path(sys.executable)
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
    if not running_on_github_actions() and not skip_linux_docker_preflight():
        if run(_linux_unit_test_command(python, *pytest_args)) != 0:
            return 1
    if run_live(_supervised_pytest_command(python, "-m", "live", *pytest_args)) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
