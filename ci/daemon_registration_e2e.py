"""Execute the v1 registration bytes, sealing, and private-file E2E contract."""

from __future__ import annotations

import subprocess
from pathlib import Path

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent


def command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "daemon-registration",
            "--test",
            "daemon_registration",
        )
    )


def main() -> int:
    return subprocess.run(command(), cwd=ROOT, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
