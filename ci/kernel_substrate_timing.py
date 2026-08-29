"""Record clean and exact-repeat kernel-substrate checks without a CI threshold."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from ci.kernel_substrate_contract import package_names
from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
COMMAND = cargo_command(
    "check",
    "-p",
    "running-process",
    "--lib",
    "--no-default-features",
    "--features",
    "kernel-substrate",
)
TREE_COMMAND = cargo_command(
    "tree",
    "-p",
    "running-process",
    "--no-default-features",
    "--features",
    "kernel-substrate",
    "--edges",
    "normal,build",
    "--prefix",
    "none",
)


def run_once(target: Path) -> tuple[int, int]:
    started = time.perf_counter_ns()
    result = subprocess.run(
        COMMAND, cwd=ROOT, env={**os.environ, "CARGO_TARGET_DIR": str(target)}, check=False
    )
    return result.returncode, time.perf_counter_ns() - started


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    target = Path(tempfile.mkdtemp(prefix="running-process-kernel-substrate-"))
    try:
        clean_rc, clean_ns = run_once(target)
        repeat_rc, repeat_ns = run_once(target)
        graph = subprocess.check_output(TREE_COMMAND, cwd=ROOT, text=True)
        payload = {
            "schema_version": 1,
            "command": COMMAND,
            "package": "running-process",
            "feature": "kernel-substrate",
            "resolved_packages": sorted(package_names(graph)),
            "target_dir": "temporary",
            "clean_ns": clean_ns,
            "incremental_ns": repeat_ns,
            "clean_exit_code": clean_rc,
            "incremental_exit_code": repeat_rc,
            "lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
            "toolchain": subprocess.check_output(cargo_command("--version"), text=True).strip(),
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return 0 if clean_rc == repeat_rc == 0 else 1
    finally:
        shutil.rmtree(target, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
