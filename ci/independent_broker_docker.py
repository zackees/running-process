"""Build locally, then verify broker accounting in a disposable Docker cgroup."""

from __future__ import annotations

import argparse
import json
import platform
import shutil
import subprocess
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def native_target(machine: str) -> str:
    if machine.lower() in {"x86_64", "amd64"}:
        return "x86_64-unknown-linux-musl"
    if machine.lower() in {"aarch64", "arm64"}:
        return "aarch64-unknown-linux-musl"
    raise ValueError(f"unsupported Docker fixture architecture: {machine}")


def artifacts(output: str) -> tuple[Path, Path]:
    """Select executables reported by this build, never stale glob matches."""
    found: dict[str, Path] = {}
    for line in output.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(record, dict) or record.get("reason") != "compiler-artifact":
            continue
        executable = record.get("executable")
        if executable:
            found[record["target"]["name"]] = Path(executable).resolve()
    try:
        return found["independent_broker_docker"], found["running-process-launcher"]
    except KeyError as error:
        raise ValueError(
            "build did not report both broker fixture executables"
        ) from error


def container_command(name: str, test: Path, launcher: Path) -> list[str]:
    # Privilege permits cgroup setup only in this disposable private namespace.
    # Never mount host cgroups or source into the container.
    return [
        "docker",
        "run",
        "--rm",
        "--name",
        name,
        "--network",
        "none",
        "--memory",
        "128m",
        "--memory-swap",
        "128m",
        "--privileged",
        "--cgroupns",
        "private",
        "--mount",
        f"type=bind,source={test},target=/fixture/test,readonly",
        "--mount",
        f"type=bind,source={launcher},target=/fixture/launcher,readonly",
        "--entrypoint",
        "/fixture/test",
        "alpine:3.20",
        "--exact",
        "docker_broker_accounting",
        "--ignored",
        "--nocapture",
    ]


def run_container(name: str, test: Path, launcher: Path) -> int:
    try:
        return subprocess.run(
            container_command(name, test, launcher), check=False, timeout=120
        ).returncode
    finally:
        # --rm handles normal exit; this handles a killed/timed-out Docker CLI.
        # The UUID name is exclusive to this invocation, not a shared resource.
        subprocess.run(
            ["docker", "rm", "--force", name],
            check=False,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


def build_fixture(target: str) -> tuple[Path, Path]:
    build = subprocess.run(
        [
            "soldr",
            "--no-cache",
            "build",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            "independent-spawn",
            "--test",
            "independent_broker_docker",
            "--bin",
            "running-process-launcher",
            "--target",
            target,
            "--message-format=json",
        ],
        cwd=ROOT,
        check=False,
        timeout=900,
        stdout=subprocess.PIPE,
        text=True,
    )
    if build.returncode:
        print(build.stdout, file=sys.stderr)
        raise SystemExit(build.returncode)
    return artifacts(build.stdout)


def stage_fixture(directory: Path, test: Path, launcher: Path) -> None:
    # A fresh directory avoids overwriting unrelated files or staging stale
    # executables alongside the current build's two reported artifacts.
    directory.mkdir(parents=True, exist_ok=False)
    shutil.copy2(test, directory / "test")
    shutil.copy2(launcher, directory / "launcher")


def verify_native_fixture(test: Path, launcher: Path) -> None:
    expected = 183 if native_target(platform.machine()).startswith("aarch64") else 62
    for executable in (test, launcher):
        with executable.open("rb") as binary:
            header = binary.read(20)
        if (
            len(header) != 20
            or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != expected
        ):
            raise ValueError(f"fixture is not a native 64-bit Linux ELF: {executable}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--build-only", type=Path, metavar="NEW_DIRECTORY")
    modes.add_argument("--run-only", type=Path, metavar="FIXTURE_DIRECTORY")
    parser.add_argument(
        "--target", choices=["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]
    )
    args = parser.parse_args(argv)
    if sys.platform != "linux":
        raise SystemExit("requires a Linux host")
    if args.run_only and args.target:
        parser.error(
            "--run-only verifies native architecture and does not accept --target"
        )
    if args.run_only:
        test, launcher = (args.run_only / "test").resolve(), (
            args.run_only / "launcher"
        ).resolve()
    else:
        if not shutil.which("soldr"):
            raise SystemExit("building requires soldr")
        test, launcher = build_fixture(args.target or native_target(platform.machine()))
        if args.build_only:
            stage_fixture(args.build_only, test, launcher)
            return 0
    if not shutil.which("docker"):
        raise SystemExit("running requires a Docker engine")
    verify_native_fixture(test, launcher)
    return run_container(f"rp-independent-{uuid.uuid4().hex}", test, launcher)


if __name__ == "__main__":
    raise SystemExit(main())
