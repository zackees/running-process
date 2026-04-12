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
TRAMPOLINE_ASSETS = ROOT / "src" / "running_process" / "assets"

BuildMode = Literal["dev", "release"]


def build_command(mode: BuildMode, *, rustc_args: list[str] | None = None) -> list[str]:
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
            cmd.extend(["--zig", "--compatibility", "manylinux2014"])
        else:
            cmd.extend(["--compatibility", "pypi"])
    if rustc_args:
        cmd.append("--")
        cmd.extend(rustc_args)
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
            "--no-deps",
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


def build_trampoline(mode: BuildMode) -> int:
    """Build the daemon-trampoline binary and copy it into package assets."""
    import json as json_mod
    import shutil

    profile_args = ["--release"] if mode == "release" else []
    result = subprocess.run(
        ["cargo", "build", "-p", "daemon-trampoline", *profile_args],
        cwd=ROOT,
        check=False,
    )
    if result.returncode != 0:
        return result.returncode

    # Query cargo for the actual target directory (may differ on CI).
    meta = subprocess.run(
        ["cargo", "metadata", "--format-version=1", "--no-deps"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    target_dir = Path(json_mod.loads(meta.stdout)["target_directory"])

    # Cargo outputs to target/<profile>/ where profile is "release" or "debug"
    # (the "dev" profile outputs to the "debug" directory).
    profile_dir = "release" if mode == "release" else "debug"
    ext = ".exe" if platform.system() == "Windows" else ""
    src = target_dir / profile_dir / f"daemon-trampoline{ext}"
    if not src.exists():
        print(f"trampoline binary not found at {src}", file=sys.stderr, flush=True)
        return 1
    dest = TRAMPOLINE_ASSETS / f"daemon-trampoline{ext}"
    TRAMPOLINE_ASSETS.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest)
    print(f"trampoline: {src} -> {dest}", file=sys.stderr, flush=True)
    return 0


def run_build(mode: BuildMode) -> int:
    from ci.env import build_env
    from ci.tiny_pdb import (
        apply_tiny_pdb_env,
        bundle_windows_tiny_pdb,
        filter_public_pdb,
        filtered_pdb_path,
        final_crate_rustc_args,
        stripped_pdb_path,
    )
    from ci.verify_release_symbols import format_release_artifact_report, verify_release_artifact

    rc = build_trampoline(mode)
    if rc != 0:
        print("trampoline build failed", file=sys.stderr, flush=True)
        return rc

    env = build_env()
    rustc_args: list[str] = []
    if mode == "release":
        env = apply_tiny_pdb_env(env)
        if platform.system() == "Windows":
            rustc_args = final_crate_rustc_args(ROOT)
    DIST.mkdir(parents=True, exist_ok=True)
    before = {path.name for path in built_wheels()}
    cmd = build_command(mode, rustc_args=rustc_args)
    print(f"build mode: {mode}", file=sys.stderr, flush=True)
    result = subprocess.run(cmd, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        return result.returncode
    if mode == "release" and platform.system() == "Windows":
        tiny_pdb = filter_public_pdb(
            source_pdb=stripped_pdb_path(ROOT),
            destination_pdb=filtered_pdb_path(ROOT),
            root=ROOT,
        )
        new_wheels = [path for path in built_wheels() if path.name not in before]
        for wheel in (new_wheels or [latest_wheel()]):
            bundled = bundle_windows_tiny_pdb(wheel, tiny_pdb=tiny_pdb, root=ROOT)
            print(
                f"bundled tiny PDB into {wheel.name}: {', '.join(bundled)}",
                file=sys.stderr,
                flush=True,
            )
            report = verify_release_artifact(wheel)
            print(format_release_artifact_report(report), file=sys.stderr, flush=True)
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
