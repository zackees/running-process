from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from ci.dev_build import ensure_dev_wheel, repo_python

ROOT = Path(__file__).resolve().parent.parent
IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"


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


def main() -> int:
    activate, _ = load_env_helpers()
    activate()
    python = repo_python()
    if os.environ.get(IN_RUNNING_PROCESS_ENV) != IN_RUNNING_PROCESS_VALUE:
        try:
            ensure_dev_wheel(python, root=ROOT)
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1
    if run(["cargo", "test", "--workspace"]) != 0:
        return 1
    if run([str(python), "-m", "pytest", "-m", "not live"]) != 0:
        return 1
    if run_live([str(python), "-m", "pytest", "-m", "live"]) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
