"""Build locally, then verify broker accounting in a disposable Docker cgroup."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


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


def main() -> int:
    if (
        sys.platform != "linux"
        or not shutil.which("soldr")
        or not shutil.which("docker")
    ):
        raise SystemExit("requires Linux with soldr and a running Docker engine")
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
            "x86_64-unknown-linux-musl",
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
        return build.returncode
    test, launcher = artifacts(build.stdout)
    return run_container(f"rp-independent-{uuid.uuid4().hex}", test, launcher)


if __name__ == "__main__":
    raise SystemExit(main())
