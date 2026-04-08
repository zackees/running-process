from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from ci.env import activate, clean_env

ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str]) -> int:
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def run_live(cmd: list[str]) -> int:
    env = clean_env()
    env["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    return subprocess.run(cmd, cwd=ROOT, env=env).returncode


def main() -> int:
    activate()
    if run(["uv", "run", "maturin", "develop", "--uv", "--profile", "dev"]) != 0:
        return 1
    if run(["cargo", "test", "--workspace"]) != 0:
        return 1
    if run(["uv", "run", "pytest", "-m", "not live"]) != 0:
        return 1
    if run_live(["uv", "run", "pytest", "-m", "live"]) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
