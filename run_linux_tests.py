from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from ci.linux_docker import (
    DEFAULT_ENGINE_TIMEOUT_SECONDS,
    docker_engine_running,
    docker_executable,
    ensure_docker_engine_running,
)

ROOT = Path(__file__).resolve().parent
BUILD_DOCKERFILE = ROOT / "Dockerfile.linux-build"
PYTEST_DOCKERFILE = ROOT / "Dockerfile.linux-pytest"
LINUX_DIR = ROOT / "linux"
BUILD_IMAGE_TAG = "running-process/linux-build:manual"
PYTEST_IMAGE_TAG = "running-process/linux-pytest:manual"
DEFAULT_PYTEST_ARGS = ["-m", "not live", "-ra"]


def parse_args(argv: list[str] | None = None) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(description="Build the Linux wheel and run Linux pytest in Docker")
    parser.add_argument(
        "--platform",
        default=None,
        help="Optional Docker platform such as linux/amd64 or linux/arm64",
    )
    parser.add_argument(
        "--engine-timeout",
        type=float,
        default=DEFAULT_ENGINE_TIMEOUT_SECONDS,
        help="Seconds to wait for Docker Desktop to bring up the engine",
    )
    parser.add_argument(
        "--no-auto-start",
        action="store_true",
        help="Do not attempt to start Docker Desktop when the engine is unavailable",
    )
    return parser.parse_known_args(argv)


def run(cmd: list[str], *, capture_output: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=ROOT,
        check=True,
        capture_output=capture_output,
        text=True,
    )


def ensure_docker(args: argparse.Namespace) -> str:
    if args.no_auto_start:
        docker = docker_executable()
        if not docker_engine_running(docker=docker):
            raise RuntimeError("docker server is not reachable; start Docker Desktop and retry")
        return docker
    return ensure_docker_engine_running(timeout_seconds=args.engine_timeout)


def build_image(docker: str, *, dockerfile: Path, tag: str, target: str | None, platform: str | None) -> None:
    cmd = [docker, "build", "-f", str(dockerfile), "-t", tag]
    if target:
        cmd.extend(["--target", target])
    if platform:
        cmd.extend(["--platform", platform])
    cmd.append(".")
    run(cmd)


def files_equal(left: Path, right: Path) -> bool:
    if not left.exists() or not right.exists():
        return False
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as left_file, right.open("rb") as right_file:
        while True:
            left_chunk = left_file.read(1024 * 1024)
            right_chunk = right_file.read(1024 * 1024)
            if left_chunk != right_chunk:
                return False
            if not left_chunk:
                return True


def promote_wheel(temp_wheel: Path, dest_dir: Path) -> Path:
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest_path = dest_dir / temp_wheel.name
    if dest_path.exists() and files_equal(temp_wheel, dest_path):
        return dest_path
    os.replace(temp_wheel, dest_path)
    return dest_path


def remove_stale_wheels(dest_dir: Path, keep_name: str) -> None:
    for wheel in dest_dir.glob("running_process-*.whl"):
        if wheel.name != keep_name:
            wheel.unlink(missing_ok=True)


def export_wheel_from_build_image(docker: str) -> Path:
    LINUX_DIR.mkdir(parents=True, exist_ok=True)
    container_id = run([docker, "create", BUILD_IMAGE_TAG], capture_output=True).stdout.strip()
    try:
        with tempfile.TemporaryDirectory(prefix=".incoming-", dir=LINUX_DIR) as temp_dir_name:
            temp_dir = Path(temp_dir_name)
            run([docker, "cp", f"{container_id}:/dist/.", str(temp_dir)])
            wheels = sorted(temp_dir.glob("running_process-*.whl"))
            if not wheels:
                raise RuntimeError("no wheel found in build container /dist output")
            if len(wheels) > 1:
                raise RuntimeError(f"expected one wheel in build output, found {len(wheels)}")
            dest_path = promote_wheel(wheels[0], LINUX_DIR)
            remove_stale_wheels(LINUX_DIR, dest_path.name)
            return dest_path
    finally:
        subprocess.run([docker, "rm", "-f", container_id], cwd=ROOT, check=False)


def run_pytest_image(docker: str, pytest_args: list[str], *, platform: str | None) -> int:
    build_image(docker, dockerfile=PYTEST_DOCKERFILE, tag=PYTEST_IMAGE_TAG, target=None, platform=platform)
    result = subprocess.run(
        [docker, "run", "--rm", "--init", PYTEST_IMAGE_TAG, *pytest_args],
        cwd=ROOT,
        check=False,
    )
    return int(result.returncode)


def main(argv: list[str] | None = None) -> int:
    args, extra_pytest_args = parse_args(argv)
    pytest_args = extra_pytest_args or list(DEFAULT_PYTEST_ARGS)
    try:
        docker = ensure_docker(args)
        build_image(docker, dockerfile=BUILD_DOCKERFILE, tag=BUILD_IMAGE_TAG, target="build", platform=args.platform)
        wheel = export_wheel_from_build_image(docker)
        print(f"linux wheel: {wheel}", file=sys.stderr, flush=True)
        return run_pytest_image(docker, pytest_args, platform=args.platform)
    except (RuntimeError, subprocess.CalledProcessError) as exc:
        print(str(exc), file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
