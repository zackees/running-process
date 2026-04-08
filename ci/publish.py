#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
from __future__ import annotations

import subprocess
import sys


def main() -> int:
    return subprocess.run(["uv", "build"]).returncode


if __name__ == "__main__":
    sys.exit(main())
