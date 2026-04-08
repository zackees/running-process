from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from ci.env import activate, clean_env

ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str]) -> int:
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def main() -> int:
    activate()
    if run(["cargo", "fmt", "--all", "--check"]) != 0:
        return 1
    if run(["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]) != 0:
        return 1
    if run(["uv", "run", "ruff", "check", "src", "tests", "ci"]) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
