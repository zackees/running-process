"""Run the public semantic-capture contract in the minimal substrate graph."""

from __future__ import annotations

import subprocess
from pathlib import Path

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent


def fixture_command() -> tuple[str, ...]:
    """Build the external test's checked-in child-process fixtures first."""
    return tuple(cargo_command("build", "-p", "testbins"))


def contract_command() -> tuple[str, ...]:
    """Run the public facade through exactly the kernal-api substrate feature."""
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "kernel-substrate",
            "--test",
            "async_semantic_capture",
        )
    )


def main() -> int:
    for command in (fixture_command(), contract_command()):
        if subprocess.run(command, cwd=ROOT, check=False).returncode:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
