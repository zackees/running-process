from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

from ci.env import repo_root

ROOT = repo_root()
BUILD_DOCKERFILE = ROOT / "Dockerfile.linux-build"
PYTEST_DOCKERFILE = ROOT / "Dockerfile.linux-pytest"
DIST_DEV = ROOT / "dist-dev"
BUILD_IMAGE_TAG = "running-process/linux-build:local"
PYTEST_IMAGE_TAG = "running-process/linux-pytest:local"
DEFAULT_ENGINE_TIMEOUT_SECONDS = 120.0
DEFAULT_PYTEST_ARGS = ["-m", "not live", "-ra"]
SERVER_VERSION_FORMAT = "{{.Server.Version}}"
WINDOWS_DOCKER_DESKTOP = Path(r"C:\Program Files\Docker\Docker\Docker Desktop.exe")


def docker_executable() -> str:
    docker = shutil.which("docker")
    if docker:
        return docker
    fallback = Path(r"C:\Program Files\Docker\Docker\resources\bin\docker.exe")
    if fallback.is_file():
        return str(fallback)
    raise RuntimeError("docker executable not found on PATH")


def docker_desktop_executable() -> Path | None:
    if os.name != "nt":
        return None
    if WINDOWS_DOCKER_DESKTOP.is_file():
        return WINDOWS_DOCKER_DESKTOP
    return None


def docker_engine_running(*, docker: str | None = None) -> bool:
    docker_cmd = docker or docker_executable()
    result = subprocess.run(
        [docker_cmd, "version", "--format", SERVER_VERSION_FORMAT],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    return result.returncode == 0 and bool(result.stdout.strip())


def start_docker_desktop(*, desktop: Path | None = None) -> None:
    executable = desktop or docker_desktop_executable()
    if executable is None:
        raise RuntimeError("Docker Desktop executable not found")
    subprocess.Popen(
        [str(executable)],
        cwd=executable.parent,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def ensure_docker_engine_running(*, timeout_seconds: float = DEFAULT_ENGINE_TIMEOUT_SECONDS) -> str:
    docker = docker_executable()
    if docker_engine_running(docker=docker):
        return docker
    desktop = docker_desktop_executable()
    if desktop is None:
        raise RuntimeError(
            "docker server is not reachable and no Docker Desktop launcher was found; "
            "start the Docker engine and retry"
        )
    start_docker_desktop(desktop=desktop)
    deadline = time.monotonic() + max(0.0, timeout_seconds)
    while time.monotonic() < deadline:
        if docker_engine_running(docker=docker):
            return docker
        time.sleep(1.0)
    raise RuntimeError(
        f"timed out after {timeout_seconds:.0f}s waiting for the Docker engine to start"
    )


def split_pytest_args(pytest_args: str | None) -> list[str]:
    if not pytest_args:
        return list(DEFAULT_PYTEST_ARGS)
    return shlex.split(pytest_args, posix=True)


def shell_join(parts: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in parts)


def cache_volume(name: str, container_path: str) -> str:
    return f"running-process-{name}:{container_path}"


def build_image_command(
    *,
    docker: str,
    dockerfile: Path,
    tag: str,
    platform: str | None,
    target: str | None = None,
) -> list[str]:
    cmd = [docker, "build", "-f", str(dockerfile), "-t", tag]
    if target:
        cmd.extend(["--target", target])
    if platform:
        cmd.extend(["--platform", platform])
    cmd.append(".")
    return cmd


def run_container_command(
    *,
    docker: str,
    image: str,
    shell_command: str,
    extra_mounts: list[str],
) -> list[str]:
    cmd = [docker, "run", "--rm", "-w", "/work", "-v", f"{ROOT}:/work"]
    for mount in extra_mounts:
        cmd.extend(["-v", mount])
    cmd.extend([image, "sh", "-lc", shell_command])
    return cmd


def output_dir(path: Path | None = None) -> Path:
    return (path or DIST_DEV).resolve()


def wheel_glob(output_path: str = "/dist-dev") -> str:
    return f"{output_path}/running_process-*.whl"


def pytest_mounts(dist_dir: Path) -> list[str]:
    return [
        f"{dist_dir}:/dist-dev",
        cache_volume("alpine-pytest-pip", "/root/.cache/pip"),
    ]


def build_export_mounts(dist_dir: Path) -> list[str]:
    return [f"{dist_dir}:/dist-dev"]


def build_export_shell_command() -> str:
    return "mkdir -p /dist-dev && cp /dist/running_process-*.whl /dist-dev/"


def pytest_shell_command(pytest_args: list[str]) -> str:
    return (
        "mkdir -p /tmp/dist && "
        + f"cp {wheel_glob()} /tmp/dist/ && "
        + shell_join(
            [
                "python",
                "-m",
                "pip",
                "install",
                "--force-reinstall",
                "/tmp/dist/running_process-*.whl",
            ]
        )
        + " && "
        + shell_join(
            [
                "python",
                "-m",
                "running_process.cli",
                "--",
                "python",
                "-m",
                "pytest",
                *pytest_args,
            ]
        )
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Alpine Linux wheel build and pytest workflows"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("build", "pytest", "all"):
        subparser = subparsers.add_parser(name)
        subparser.add_argument(
            "--platform",
            default=None,
            help="Optional Docker platform such as linux/amd64 or linux/arm64",
        )
        subparser.add_argument(
            "--output-dir",
            type=Path,
            default=None,
            help="Directory to write wheel artifacts to; defaults to dist-dev/",
        )
        subparser.add_argument(
            "--engine-timeout",
            type=float,
            default=DEFAULT_ENGINE_TIMEOUT_SECONDS,
            help="Seconds to wait for Docker Desktop to bring up the engine",
        )
        subparser.add_argument(
            "--no-auto-start",
            action="store_true",
            help="Do not attempt to start Docker Desktop when the engine is unavailable",
        )
    subparsers.choices["pytest"].add_argument(
        "--pytest-args",
        default=None,
        help="Optional pytest arguments passed at container runtime",
    )
    subparsers.choices["all"].add_argument(
        "--pytest-args",
        default=None,
        help="Optional pytest arguments passed at container runtime",
    )
    return parser.parse_args(argv)


def run_logged(cmd: list[str]) -> int:
    return int(subprocess.run(cmd, cwd=ROOT, check=False).returncode)


def build_image(*, docker: str, dockerfile: Path, tag: str, platform: str | None) -> int:
    return run_logged(
        build_image_command(
            docker=docker,
            dockerfile=dockerfile,
            tag=tag,
            platform=platform,
        )
    )


def ensure_wheel_exists(dist_dir: Path) -> None:
    wheels = sorted(dist_dir.glob("running_process-*.whl"))
    if not wheels:
        raise RuntimeError(
            f"no wheel found in {dist_dir}; run `python -m ci.linux_build_wheel` first"
        )


def run_build_workflow(*, docker: str, platform: str | None, dist_dir: Path) -> int:
    dist_dir.mkdir(parents=True, exist_ok=True)
    if (
        run_logged(
            build_image_command(
                docker=docker,
                dockerfile=BUILD_DOCKERFILE,
                tag=BUILD_IMAGE_TAG,
                platform=platform,
                target="build",
            )
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=BUILD_IMAGE_TAG,
            shell_command=build_export_shell_command(),
            extra_mounts=build_export_mounts(dist_dir),
        )
    )


def run_pytest_workflow(
    *,
    docker: str,
    platform: str | None,
    dist_dir: Path,
    pytest_args: list[str],
) -> int:
    ensure_wheel_exists(dist_dir)
    if (
        build_image(
            docker=docker,
            dockerfile=PYTEST_DOCKERFILE,
            tag=PYTEST_IMAGE_TAG,
            platform=platform,
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=PYTEST_IMAGE_TAG,
            shell_command=pytest_shell_command(pytest_args),
            extra_mounts=pytest_mounts(dist_dir),
        )
    )


def resolve_docker(args: argparse.Namespace) -> str:
    if args.no_auto_start:
        docker = docker_executable()
        if not docker_engine_running(docker=docker):
            raise RuntimeError("docker server is not reachable; start Docker Desktop and retry")
        return docker
    return ensure_docker_engine_running(timeout_seconds=args.engine_timeout)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    dist_dir = output_dir(args.output_dir)
    try:
        docker = resolve_docker(args)
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr, flush=True)
        return 1

    if args.command == "build":
        return run_build_workflow(docker=docker, platform=args.platform, dist_dir=dist_dir)

    if args.command == "pytest":
        try:
            return run_pytest_workflow(
                docker=docker,
                platform=args.platform,
                dist_dir=dist_dir,
                pytest_args=split_pytest_args(args.pytest_args),
            )
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    if args.command == "all":
        build_result = run_build_workflow(docker=docker, platform=args.platform, dist_dir=dist_dir)
        if build_result != 0:
            return build_result
        try:
            return run_pytest_workflow(
                docker=docker,
                platform=args.platform,
                dist_dir=dist_dir,
                pytest_args=split_pytest_args(args.pytest_args),
            )
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    raise RuntimeError(f"unsupported command {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
