"""Validate the isolated probe facade dependency graph before Rust tests run."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from ci.soldr import cargo_command


def validate(output: str) -> str:
    """Reject missing, ambiguous or feature-bearing probe graph evidence."""
    lines = [line for line in output.splitlines() if "running-process-probe" in line]
    if any("running-process-probe-daemon" in line for line in lines):
        raise RuntimeError("probe daemon must not be present in the facade graph")
    probe = [line for line in lines if "running-process-probe v" in line]
    if len(probe) != 1 or "::" not in probe[0]:
        raise RuntimeError("probe dependency graph is missing or ambiguous")
    features = probe[0].rsplit("::", 1)[1].strip()
    if features:
        raise RuntimeError(f"probe dependency unexpectedly enables features: {features}")
    return "\n".join(lines)


def check(target: str | None = "all") -> str:
    command = cargo_command(
        "tree", "-p", "running-process", "--features", "probe",
        "--no-default-features", "--edges", "normal", "-f", "{p} :: {f}",
    )
    if target:
        command.extend(["--target", target])
    result = subprocess.run(
        command, cwd=Path(__file__).resolve().parent.parent,
        capture_output=True, text=True, check=False,
    )
    if result.returncode != 0:
        raise RuntimeError("probe dependency fixture failed: " + result.stderr[-2048:])
    return validate(result.stdout)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="all")
    args = parser.parse_args()
    try:
        check(args.target)
    except (OSError, RuntimeError) as exc:
        print(str(exc), file=sys.stderr, flush=True)
        return 1
    print("probe dependency graph contract passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
