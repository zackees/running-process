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

Known local caveat: with soldr's shims first on `PATH`, `cargo dylint` can build
the lint library and then fail to find it (zackees/soldr#3483). That is a soldr
bug and is deliberately not worked around here.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

NIGHTLY = "nightly-2026-04-16"
DYLINT_VERSION = "6.0.1"
REQUIRE_ENV = "RUNNING_PROCESS_REQUIRE_DYLINT"
# Keep Dylint's nightly artifacts apart from stable Clippy's target directory.
TARGET_DIR = ROOT / "target" / "dylint"


def commands() -> list[list[str]]:
    """The two workspace gates, identical to the CI `dylint` job."""
    base = ["rustup", "run", NIGHTLY, "cargo", "dylint"]
    return [
        [*base, "--all", "--workspace"],
        [
            *base,
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
    result = subprocess.run(
        ["rustup", "toolchain", "list"], capture_output=True, text=True, check=False
    )
    return result.returncode == 0 and any(
        line.startswith(NIGHTLY) for line in result.stdout.splitlines()
    )


def missing_tools() -> list[str]:
    missing = []
    if not toolchain_installed():
        missing.append(
            f"rustup toolchain install {NIGHTLY} --profile minimal "
            "-c rustc-dev -c llvm-tools-preview"
        )
    if not shutil.which("cargo-dylint") or not shutil.which("dylint-link"):
        missing.append(
            f"cargo install cargo-dylint@{DYLINT_VERSION} "
            f"dylint-link@{DYLINT_VERSION} --locked"
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
    env = {**os.environ, "CARGO_TARGET_DIR": str(TARGET_DIR)}
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
