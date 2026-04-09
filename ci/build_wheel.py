#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import argparse
import contextlib
import platform
import subprocess
import sys
from pathlib import Path
from typing import Literal

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"

BuildMode = Literal["dev", "release"]


def build_command(mode: BuildMode) -> list[str]:
    cmd = [
        sys.executable,
        "-m",
        "maturin",
    ]
    cmd.extend(
        [
            "build",
            "--interpreter",
            sys.executable,
            "--out",
            str(DIST),
        ]
    )
    if mode == "dev":
        cmd.extend(["--profile", "dev"])
    else:
        cmd.append("--release")
        if platform.system() == "Linux":
            cmd.extend(["--compatibility", "manylinux2014"])
        else:
            cmd.extend(["--compatibility", "pypi"])
    return cmd


def built_wheels() -> list[Path]:
    return sorted(DIST.glob("running_process-*.whl"), key=lambda path: path.stat().st_mtime)


def latest_wheel() -> Path:
    wheels = built_wheels()
    if not wheels:
        raise RuntimeError(f"no built wheel found in {DIST}")
    return wheels[-1]


def install_wheel(wheel: Path, *, env: dict[str, str]) -> int:
    install = subprocess.run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            sys.executable,
            "--reinstall",
            str(wheel),
        ],
        cwd=ROOT,
        check=False,
        env=env,
    )
    if install.returncode != 0:
        return install.returncode

    # Clean up the stale editable path file if a prior `maturin develop` left one behind.
    for pth in (ROOT / ".venv").glob("**/site-packages/running_process.pth"):
        with contextlib.suppress(OSError):
            pth.unlink()
    return 0


def run_build(mode: BuildMode) -> int:
    from ci.env import build_env

    env = build_env()
    DIST.mkdir(parents=True, exist_ok=True)
    before = {path.name for path in built_wheels()}
    cmd = build_command(mode)
    print(f"build mode: {mode}", file=sys.stderr, flush=True)
    result = subprocess.run(cmd, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        return result.returncode
    if mode != "dev":
        return 0

    wheel = latest_wheel()
    action = "reinstalling existing dev wheel" if wheel.name in before else "installing dev wheel"
    print(f"{action}: {wheel.name}", file=sys.stderr, flush=True)
    return install_wheel(wheel, env=env)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build running-process")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--dev",
        action="store_true",
        help="build a dev-profile wheel and reinstall it into the active uv environment",
    )
    mode.add_argument(
        "--quick",
        action="store_true",
        help="alias for --dev",
    )
    mode.add_argument(
        "--release",
        action="store_true",
        help="build release wheel(s) into dist/",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, *, default_mode: BuildMode = "release") -> int:
    args = parse_args(argv)
    mode: BuildMode = default_mode
    if args.dev or args.quick:
        mode = "dev"
    if args.release:
        mode = "release"
    return run_build(mode)


if __name__ == "__main__":
    sys.exit(main())
