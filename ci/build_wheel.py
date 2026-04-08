#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import argparse
import platform
import shutil
import subprocess
import sys
import sysconfig
from pathlib import Path
from typing import Literal

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

BuildMode = Literal["dev", "release"]


def build_command(mode: BuildMode) -> list[str]:
    cmd = ["uv", "run"]
    if mode == "dev":
        cmd.extend(["maturin", "develop", "--uv", "--profile", "dev"])
        return cmd

    if platform.system() == "Linux":
        cmd.extend(["--with", "ziglang"])
    cmd.extend(
        [
            "maturin",
            "build",
            "--release",
            "--interpreter",
            sys.executable,
            "--out",
            str(ROOT / "dist"),
        ]
    )
    if platform.system() == "Linux":
        cmd.extend(["--compatibility", "manylinux2014", "--zig"])
    else:
        cmd.extend(["--compatibility", "pypi"])
    return cmd


def sync_in_tree_native_artifact() -> None:
    ext_suffix = sysconfig.get_config_var("EXT_SUFFIX")
    if not isinstance(ext_suffix, str) or not ext_suffix:
        return

    source_name = "_native.dll" if platform.system() == "Windows" else f"_native{ext_suffix}"
    source = ROOT / "target" / "maturin" / source_name
    if not source.is_file():
        return

    destination = ROOT / "src" / "running_process" / f"_native{ext_suffix}"
    try:
        shutil.copy2(source, destination)
    except PermissionError:
        if platform.system() != "Windows":
            raise
        print(
            (
                f"warning: could not refresh in-tree native artifact at {destination} "
                "because it is currently in use; the editable install was updated successfully"
            ),
            file=sys.stderr,
            flush=True,
        )


def run_build(mode: BuildMode) -> int:
    from ci.env import build_env

    env = build_env(use_zccache=mode == "dev")
    cmd = build_command(mode)
    print(f"build mode: {mode}", file=sys.stderr, flush=True)
    if mode == "dev":
        print(
            f"zccache: {env.get('RUSTC_WRAPPER', 'disabled')}",
            file=sys.stderr,
            flush=True,
        )
    result = subprocess.run(cmd, cwd=ROOT, check=False, env=env)
    if result.returncode == 0 and mode == "dev":
        sync_in_tree_native_artifact()
    return result.returncode


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build running-process")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--dev",
        action="store_true",
        help="fast local editable rebuild using maturin develop --uv --profile dev",
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
