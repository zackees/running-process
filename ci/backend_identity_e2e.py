"""Execute the direct backend-identity public and IPC contracts."""

from __future__ import annotations

import subprocess
from pathlib import Path

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent


def public_contract_command() -> tuple[str, ...]:
    """Run the facade/mux/sidecar/golden tests without the client feature."""
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "backend-identity",
            "--test",
            "backend_identity",
        )
    )


def ipc_e2e_command() -> tuple[str, ...]:
    """Run an accepted-IPC probe/responder round trip in the same graph."""
    return tuple(
        cargo_command(
            "test",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "backend-identity",
            "--lib",
            "backend_identity::direct_probe_e2e_tests",
        )
    )


def main() -> int:
    for command in (public_contract_command(), ipc_e2e_command()):
        if subprocess.run(command, cwd=ROOT, check=False).returncode:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
