"""Execute the v2 service-definition writer E2E under minimal features."""

from __future__ import annotations

import subprocess
from pathlib import Path

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent


def minimal_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "daemon-registration-v2",
            "--test",
            "daemon_registration_v2",
        )
    )


def coexistence_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "daemon-registration,daemon-registration-v2",
            "--test",
            "daemon_registration_v2",
        )
    )


def main() -> int:
    for command in (minimal_command(), coexistence_command()):
        if subprocess.run(command, cwd=ROOT, check=False).returncode:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
