"""Drive instrumented runpm/daemon binaries on an isolated Linux CI runner."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def _run(binary: Path, *args: str, env: dict[str, str]) -> str:
    command = [str(binary), *args]
    result = subprocess.run(
        command,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    print(f"$ {binary.name} {' '.join(args)} -> {result.returncode}", flush=True)
    if result.stdout:
        print(result.stdout, end="", flush=True)
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr, flush=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"{binary.name} {' '.join(args)} exited {result.returncode}"
        )
    return result.stdout


def exercise_runpm(binary: Path) -> None:
    """Exercise safe lifecycle commands against an isolated real daemon."""
    with tempfile.TemporaryDirectory(prefix="running-process-coverage-") as raw_root:
        root = Path(raw_root)
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(root / "home"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_RUNTIME_DIR": str(root / "runtime"),
                "XDG_STATE_HOME": str(root / "state"),
            }
        )
        for name in ("home", "config", "runtime", "state"):
            (root / name).mkdir(mode=0o700)

        service = "coverage-smoke"
        sleeper = (
            "import os,time; "
            "print('coverage-child:' + os.environ['RP_COVERAGE']); "
            "time.sleep(20)"
        )
        try:
            _run(binary, "kill", env=env)
            _run(binary, "--start-daemon", env=env)
            _run(binary, "ping", env=env)
            _run(
                binary,
                "start",
                "--name",
                service,
                "--env",
                "RP_COVERAGE=live",
                "--no-autorestart",
                "--",
                sys.executable,
                "-u",
                "-c",
                sleeper,
                env=env,
            )
            time.sleep(0.25)
            _run(binary, "list", env=env)
            _run(binary, "list", "--json", env=env)
            _run(binary, "show", service, env=env)
            _run(binary, "logs", service, "--lines", "20", env=env)
            _run(binary, "flush", service, env=env)
            _run(binary, "stop", service, env=env)
            _run(binary, "restart", service, env=env)
            _run(binary, "save", env=env)
            _run(binary, "delete", service, env=env)
            _run(binary, "resurrect", env=env)
            _run(binary, "stop", service, env=env)
            _run(binary, "delete", service, env=env)
            _run(
                binary,
                "maintenance",
                "release-handles",
                "--path",
                str(root),
                "--json",
                env=env,
            )
        finally:
            _run(binary, "kill", env=env)
            # The shutdown acknowledgement precedes process exit. Give the
            # daemon time to return from main and flush its LLVM profile.
            time.sleep(0.5)


def main() -> int:
    if not sys.platform.startswith("linux"):
        raise RuntimeError("coverage process smoke is isolated for Linux CI only")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    args = parser.parse_args()
    binary = args.bin_dir / "runpm"
    if not binary.is_file():
        raise RuntimeError(f"instrumented runpm binary not found at {binary}")
    exercise_runpm(binary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
