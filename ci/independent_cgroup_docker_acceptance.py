"""Run acceptance in an explicitly preprovisioned local Docker container.

No container creation, host delegation, privileged mode, or container teardown.
The operator supplies a cgroupfs-driver container with a finite memory limit,
host cgroup namespace, and an empty writable delegated child below its actual
container cgroup. Source and the entire prebuilt profile must already be mounted
read-only at the supplied container paths. Unsupported topologies fail closed.
"""
from __future__ import annotations

import argparse
import json
import math
import re
import subprocess
from pathlib import Path


def container_boundary(identity: str, pid: int, membership: str) -> Path:
    """Identify the exact cgroupfs Docker boundary, not a finite host ancestor."""
    if not re.fullmatch(r"[0-9a-f]{64}", identity) or pid <= 0:
        raise RuntimeError("invalid running container identity")
    entries = [line[3:] for line in membership.splitlines() if line.startswith("0::")]
    if len(entries) != 1:
        raise RuntimeError("local container must use unified cgroup v2")
    path = Path(entries[0])
    if not path.is_absolute() or ".." in path.parts:
        raise RuntimeError("ambiguous host cgroup membership")
    matches = [index for index, part in enumerate(path.parts) if part == identity]
    if len(matches) != 1:
        raise RuntimeError("only an identifiable cgroupfs Docker boundary is supported")
    return Path("/sys/fs/cgroup").joinpath(*path.parts[1:matches[0] + 1])


def exec_argv(docker: list[str], identity: str, runner: Path, helper: Path,
              test: Path, parent: Path, outer: Path) -> list[str]:
    for path in (runner, helper, test, parent, outer):
        if not path.is_absolute() or ".." in path.parts:
            raise ValueError("container paths must be absolute without parent traversal")
    return [*docker, "exec", identity, "python3", str(runner),
            "--cgroup-parent", str(parent), "--container-cgroup", str(outer),
            "--helper", str(helper), "--test-binary", str(test)]


def inspect_running(docker: list[str], reference: str) -> dict:
    result = subprocess.run(
        [*docker, "inspect", "--type", "container", "--", reference],
        capture_output=True, text=True, timeout=15, check=True,
    )
    records = json.loads(result.stdout)
    if not isinstance(records, list) or len(records) != 1:
        raise RuntimeError("container inspection did not return exactly one object")
    record = records[0]
    if record["State"]["Running"] is not True or record["State"].get("Paused", False):
        raise RuntimeError("acceptance container must be running and unpaused")
    return record


def run() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--container", required=True)
    parser.add_argument("--docker-socket", type=Path, default=Path("/var/run/docker.sock"))
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--cgroup-parent", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--test-binary", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=900)
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    socket = args.docker_socket.resolve(strict=True)
    if not socket.is_socket():
        parser.error("--docker-socket must identify a local Docker engine socket")
    docker = ["docker", "--host", f"unix://{socket}"]
    record = inspect_running(docker, args.container)
    identity = record["Id"]
    pid = record["State"]["Pid"]
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
        raise RuntimeError("container has no valid host PID")
    membership = Path(f"/proc/{pid}/cgroup").read_text()
    outer = container_boundary(identity, pid, membership).resolve(strict=True)
    parent = args.cgroup_parent.resolve(strict=True)
    if parent == outer or not parent.is_relative_to(outer):
        raise RuntimeError("delegation must be strictly inside this container")
    limit = (outer / "memory.max").read_text().strip()
    if not limit.isdecimal() or int(limit) <= 0:
        raise RuntimeError("Docker container must have a finite positive memory limit")
    # Pin identity and launch epoch again after host observations. Never adopt a
    # replacement container by name or quietly continue after a restart.
    confirmed = inspect_running(docker, identity)
    if (confirmed["Id"], confirmed["State"]["Pid"], confirmed["State"]["StartedAt"]) != (
        identity, pid, record["State"]["StartedAt"]
    ):
        raise RuntimeError("container restarted during topology inspection")
    command = exec_argv(docker, identity, args.runner, args.helper,
                        args.test_binary, parent, outer)
    try:
        subprocess.run(command, check=True, timeout=args.timeout)
    except subprocess.TimeoutExpired as error:
        # Killing the Docker CLI does not prove remote exec termination.
        # Do not stop an operator-owned container or delete its diagnostics.
        raise RuntimeError(
            f"acceptance observation timed out in {identity}; remote cleanup "
            "is unconfirmed. Container and runner artifacts are retained"
        ) from error
    completed = inspect_running(docker, identity)
    if (completed["Id"], completed["State"]["Pid"], completed["State"]["StartedAt"]) != (
        identity, pid, record["State"]["StartedAt"]
    ):
        raise RuntimeError("container restarted during acceptance; result is invalid")
    if Path(f"/proc/{pid}/cgroup").read_text() != membership:
        raise RuntimeError("container init changed cgroup during acceptance; result is invalid")
    if (outer / "memory.max").read_text().strip() != limit:
        raise RuntimeError("container memory limit changed during acceptance; result is invalid")


if __name__ == "__main__":
    run()
