from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def repo_python() -> Path:
    if sys.platform == "win32":
        return ROOT / ".venv" / "Scripts" / "python.exe"
    return ROOT / ".venv" / "bin" / "python"


def run(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def main() -> int:
    activate, _ = load_env_helpers()
    activate()
    python = repo_python()
    if run([str(python), "-m", "ci.spawn_path_guard"]) != 0:
        return 1
    if run(["cargo", "fmt", "--all"]) != 0:
        return 1
    if run(["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]) != 0:
        return 1
    if run([str(python), "-m", "ruff", "check", "--fix", "src", "tests", "ci"]) != 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
