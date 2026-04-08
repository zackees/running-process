#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from ci.env import clean_env


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        print("usage: run_logged.py <log-path> -- <command...>", file=sys.stderr)
        return 2

    log_path = Path(sys.argv[1])
    command = sys.argv[3:]
    log_path.parent.mkdir(parents=True, exist_ok=True)

    with log_path.open("w", encoding="utf-8", errors="replace") as handle:
        process = subprocess.Popen(
            command,
            env=clean_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        assert process.stdout is not None
        for line in process.stdout:
            sys.stdout.write(line)
            handle.write(line)
        return process.wait()


if __name__ == "__main__":
    sys.exit(main())
