"""Local Dylint gate: both repo-owned lint libraries over the workspace.

CI runs the same two commands in the `dylint` job of `ci-preflight.yml`. Dylint
needs a pinned nightly with `rustc-dev` plus `cargo-dylint`, which most
contributors do not have, so a missing toolchain skips with instructions
instead of failing `./lint`. Set `RUNNING_PROCESS_REQUIRE_DYLINT=1` to make a
missing toolchain a failure (CI-like local runs).

This is host Dylint. It is not cross-target: the platform-boundary lint runs
pre-expansion and sees cfg-inactive OS branches, but the env-literal lint is a
late lint and only sees the module graph selected for this host. soldr's
cross-target Clippy does not change that.

Use Soldr's managed Dylint front door and preserve the lint crates' declared
linker. Their cfg(all()) linker selection is not detected by Soldr's automatic
linker resolver (zackees/soldr#3483); SOLDR_LINKER=default retains dylint-link.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

NIGHTLY = "nightly-2026-05-28"
DYLINT_VERSION = "6.0.3"
REQUIRE_ENV = "RUNNING_PROCESS_REQUIRE_DYLINT"
# Keep Dylint's nightly artifacts apart from stable Clippy's target directory.
TARGET_DIR = ROOT / "target" / "dylint"


def commands() -> list[list[str]]:
    """The two workspace gates, identical to the CI `dylint` job."""
    base = ["soldr", "dylint"]
    return [
        [*base, "--all", "--workspace"],
        [
            *base,
            "--all",
            "--path",
            "lints",
            "--pattern",
            "running-process-platform-boundary",
            "--workspace",
        ],
    ]


def toolchain_installed() -> bool:
    if not shutil.which("rustup"):
        return False
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as output:
        result = subprocess.run(
            ["rustup", "toolchain", "list"],
            stdout=output,
            stderr=subprocess.STDOUT,
            check=False,
        )
        output.seek(0)
        return result.returncode == 0 and any(
            line.startswith(NIGHTLY) for line in output.read().splitlines()
        )


def missing_tools() -> list[str]:
    missing = []
    if not shutil.which("soldr"):
        missing.append(f"install Soldr 0.9.29 or newer (managed Dylint {DYLINT_VERSION})")
    # The managed front door resolves tool binaries itself; they need not
    # appear on the caller's PATH after a successful `soldr dylint prepare`.
    if not toolchain_installed():
        missing.append(
            f"SOLDR_DYLINT_TOOLCHAIN={NIGHTLY} "
            "SOLDR_DYLINT_DRIVER_FALLBACK=off soldr dylint prepare"
        )
    return missing


def main() -> int:
    missing = missing_tools()
    if missing:
        required = os.environ.get(REQUIRE_ENV) == "1"
        print(
            "dylint: "
            + ("FAILED" if required else "skipped")
            + " - the pinned Dylint toolchain is not installed. Install:\n  "
            + "\n  ".join(missing),
            file=sys.stderr,
            flush=True,
        )
        return 1 if required else 0
    env = {
        **os.environ,
        "CARGO_TARGET_DIR": str(TARGET_DIR),
        "SOLDR_DYLINT_TOOLCHAIN": NIGHTLY,
        "SOLDR_DYLINT_DRIVER_FALLBACK": "off",
        "SOLDR_LINKER": "default",
    }
    for command in commands():
        if subprocess.run(command, cwd=ROOT, env=env, check=False).returncode != 0:
            print(
                "dylint: FAILED. Either a custom lint fired (it gates CI too, so "
                "fix the violation) or the Dylint driver could not run; the "
                "output above says which.",
                file=sys.stderr,
                flush=True,
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
