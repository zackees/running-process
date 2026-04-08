#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import platform
import subprocess
import sys
from pathlib import Path


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    cmd = ["uv", "run"]
    if platform.system() == "Linux":
        cmd.extend(["--with", "ziglang"])
    cmd.extend(
        [
            "maturin",
            "build",
            "--release",
            "--interpreter",
            sys.executable,
            "--out",
            str(root / "dist"),
        ]
    )
    if platform.system() == "Linux":
        cmd.extend(["--compatibility", "manylinux2014", "--zig"])
    else:
        cmd.extend(["--compatibility", "pypi"])
    return subprocess.run(cmd, cwd=root, check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
