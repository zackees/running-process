"""No-systemd, prebuilt acceptance runner for a delegated Linux container.

Not a Docker launcher or builder. The caller supplies a writable, empty cgroup
parent with memory delegation and a shared PID/mount namespace. All destructive
controls belong to fresh cgroups created here, never the supplied parent.
"""
from __future__ import annotations

import argparse
import os
import shutil
import stat
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

REQUIRED_TESTS = frozenset({
    "inherited_daemon_retains_callers_cgroup",
    "independent_daemon_is_not_in_callers_cgroup_subtree",
    "independent_daemon_performs_work_after_launcher_exit",
    "independent_daemon_performs_work_after_owned_cgroup_kill",
    "inherited_daemon_is_removed_by_owned_cgroup_kill",
    "independent_allocation_is_charged_outside_caller_group",
    "unavailable_broker_does_not_fall_back_to_inherited_spawn",
})
NEGATIVE_TEST = "unavailable_broker_does_not_fall_back_to_inherited_spawn"


def require_directory_identity(path: Path, identity: tuple[int, int]) -> None:
    observed = path.lstat()
    if not stat.S_ISDIR(observed.st_mode) or (observed.st_dev, observed.st_ino) != identity:
        raise OSError("owned cgroup directory was replaced")


def write_control(directory_fd: int, name: str, value: bytes) -> None:
    if name not in {"memory.max", "cgroup.subtree_control"}:
        raise ValueError("unsupported acceptance control")
    fd = os.open(name, os.O_WRONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
    try:
        if os.write(fd, value) != len(value):
            raise OSError("short cgroup control write")
    finally:
        os.close(fd)


def group_is_empty(directory_fd: int) -> bool:
    fd = os.open("cgroup.events", os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
    with os.fdopen(fd, encoding="ascii") as events:
        return "populated 0" in events.read().splitlines()


def remove_empty_cgroup_children(directory_fd: int) -> None:
    """Remove empty child groups, never control files or linked directories."""
    for name in os.listdir(directory_fd):
        metadata = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if stat.S_ISLNK(metadata.st_mode):
            raise OSError("unexpected link in owned cgroup")
        if not stat.S_ISDIR(metadata.st_mode):
            continue
        child_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                           dir_fd=directory_fd)
        try:
            opened = os.fstat(child_fd)
            identity = (opened.st_dev, opened.st_ino)
            if identity != (metadata.st_dev, metadata.st_ino):
                raise OSError("child cgroup changed during cleanup")
            remove_empty_cgroup_children(child_fd)
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != identity:
                raise OSError("child cgroup replaced before removal")
            os.rmdir(name, dir_fd=directory_fd)
        finally:
            os.close(child_fd)


def require_acceptance_binary(test: Path) -> None:
    result = subprocess.run(
        [str(test), "--list"], check=True, capture_output=True, text=True, timeout=10,
    )
    discovered = {line.removesuffix(": test") for line in result.stdout.splitlines()
                  if line.endswith(": test")}
    missing = REQUIRED_TESTS - discovered
    if missing:
        raise RuntimeError("acceptance binary is missing tests: " + ", ".join(sorted(missing)))
    # Match the profile/deps lookup used inside the Rust acceptance binary.
    for name in ("testbin-independent-launcher", "testbin-independent-memory-holder"):
        fixture = test.parent.parent / name
        if not fixture.is_file() or not os.access(fixture, os.X_OK):
            raise RuntimeError("required prebuilt fixture is missing: " + name)


def child_exec() -> None:
    """Join an inherited owned control FD before exec, without preexec_fn."""
    if len(sys.argv) < 4:
        raise ValueError("child gate requires an owned control FD and executable")
    fd = int(sys.argv[2])
    pid = str(os.getpid()).encode("ascii")
    try:
        if os.write(fd, pid) != len(pid):
            raise OSError("incomplete child cgroup placement")
    finally:
        os.close(fd)
    os.execv(sys.argv[3], sys.argv[3:])


def spawn(group_fd: int, command: list[str], env: dict[str, str]) -> subprocess.Popen:
    fd = os.open("cgroup.procs", os.O_WRONLY | os.O_NOFOLLOW, dir_fd=group_fd)
    try:
        return subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--child", str(fd), *command],
            pass_fds=(fd,), env=env, stdin=subprocess.DEVNULL,
        )
    finally:
        os.close(fd)


def require_container_boundary(parent: Path, outer: Path) -> None:
    """Verify a preprovisioned container boundary without moving processes.

    The Docker orchestrator must identify the container's actual cgroup, not
    merely supply an arbitrary finite host ancestor. This second, in-container
    check rejects namespace/path mismatches and sibling delegation outside it.
    """
    outer = outer.resolve(strict=True)
    mount = Path("/sys/fs/cgroup")
    if outer == mount or not outer.is_relative_to(mount):
        raise RuntimeError("container boundary must be below the cgroup mount")
    if parent == outer or not parent.is_relative_to(outer):
        raise RuntimeError("delegated parent must be strictly inside the container boundary")
    memberships = [
        line[3:]
        for line in Path("/proc/self/cgroup").read_text().splitlines()
        if line.startswith("0::")
    ]
    if len(memberships) != 1:
        raise RuntimeError("container acceptance requires unified cgroup v2")
    relative = Path(memberships[0])
    if not relative.is_absolute() or ".." in relative.parts:
        raise RuntimeError("cgroup namespace does not expose an unambiguous membership")
    current = (mount / str(relative).lstrip("/")).resolve(strict=True)
    if not current.is_relative_to(outer):
        raise RuntimeError("runner is outside the declared container boundary")
    if str(os.getpid()) not in (current / "cgroup.procs").read_text().split():
        raise RuntimeError("cgroup mount and PID namespace membership disagree")
    limit = (outer / "memory.max").read_text().strip()
    if not limit.isdecimal() or int(limit) <= 0:
        raise RuntimeError("container boundary must have a finite positive memory.max")


def run() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cgroup-parent", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--test-binary", type=Path, required=True)
    parser.add_argument(
        "--container-cgroup", type=Path,
        help="actual Docker container cgroup, identified by the host orchestrator",
    )
    args = parser.parse_args()
    parent = args.cgroup_parent.resolve(strict=True)
    if not parent.is_relative_to("/sys/fs/cgroup"):
        raise RuntimeError("parent must be in the visible cgroup v2 filesystem")
    if "memory" not in (parent / "cgroup.subtree_control").read_text().split():
        raise RuntimeError("parent must already delegate memory; runner will not alter it")
    if args.container_cgroup is not None:
        require_container_boundary(parent, args.container_cgroup)
    helper = args.helper.resolve(strict=True)
    test = args.test_binary.resolve(strict=True)
    for executable in (helper, test):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise RuntimeError("prebuilt executable is unavailable")
    require_acceptance_binary(test)
    root = parent / ("rp-acceptance-" + uuid.uuid4().hex)
    root.mkdir()  # Never adopt an existing group.
    metadata = root.lstat()
    identity = (metadata.st_dev, metadata.st_ino)
    root_fd: int | None = None
    try:
        root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        opened = os.fstat(root_fd)
        if (opened.st_dev, opened.st_ino) != identity:
            raise OSError("new cgroup directory was replaced before control acquisition")
        kill_fd = os.open("cgroup.kill", os.O_WRONLY | os.O_NOFOLLOW, dir_fd=root_fd)
    except OSError:
        if root_fd is not None:
            os.close(root_fd)
        require_directory_identity(root, identity)
        root.rmdir()  # No process has been launched or moved yet.
        raise
    children: list[subprocess.Popen] = []
    group_fds: list[int] = []
    temporary: str | None = None
    try:
        require_directory_identity(root, identity)
        write_control(root_fd, "memory.max", str(512 * 1024 * 1024).encode("ascii"))
        write_control(root_fd, "cgroup.subtree_control", b"+memory")
        for name in ("broker", "worker"):
            os.mkdir(name, dir_fd=root_fd)
            metadata = os.stat(name, dir_fd=root_fd, follow_symlinks=False)
            fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
            group_fds.append(fd)
            opened = os.fstat(fd)
            if (metadata.st_dev, metadata.st_ino) != (opened.st_dev, opened.st_ino):
                raise OSError("new child cgroup was replaced")
        broker_group, worker_group = group_fds
        temporary = tempfile.mkdtemp(prefix="rp-broker-")
        socket = Path(temporary) / "broker.sock"
        env = dict(os.environ)
        for key in tuple(env):
            if key.startswith(("RUNNING_PROCESS_INDEPENDENT_", "RUST_TEST_")):
                del env[key]
        env.update(
            RUNNING_PROCESS_INDEPENDENT_BROKER=str(socket),
            RUNNING_PROCESS_INDEPENDENT_HELPER=str(helper),
            RUNNING_PROCESS_LIVE_TESTS="1",
            RUNNING_PROCESS_INDEPENDENT_LIVE_TESTS="1",
            RUNNING_PROCESS_INDEPENDENT_MEMORY_TESTS="1",
            RUNNING_PROCESS_INDEPENDENT_TEARDOWN_TESTS="1",
            RUNNING_PROCESS_INDEPENDENT_OUTER_CGROUP=str(root),
            RUNNING_PROCESS_TEST_CGROUP_PARENT=str(root),
        )
        broker = spawn(broker_group, [str(helper), "--broker", str(socket)], env)
        children.append(broker)
        deadline = time.monotonic() + 10
        while not socket.exists():
            if broker.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("broker failed to become ready")
            time.sleep(0.02)
        # Select every required case explicitly, in its own process. New or
        # unrelated tests in the binary cannot silently widen this harness.
        for name in sorted(REQUIRED_TESTS - {NEGATIVE_TEST}):
            worker = spawn(worker_group, [str(test), "--exact", name, "--test-threads=1"], env)
            children.append(worker)
            if worker.wait(timeout=90) != 0:
                raise RuntimeError("acceptance case failed: " + name)
        env["RUNNING_PROCESS_INDEPENDENT_BROKER"] = str(Path(temporary) / "absent.sock")
        env["RUNNING_PROCESS_INDEPENDENT_ABSENT_BROKER_TESTS"] = "1"
        negative = spawn(worker_group, [str(test), "--exact", NEGATIVE_TEST,
            "--test-threads=1"], env)
        children.append(negative)
        if negative.wait(timeout=30) != 0:
            raise RuntimeError("missing-broker acceptance failed")
    finally:
        cleanup_errors: list[str] = []
        try:
            if os.pwrite(kill_fd, b"1", 0) != 1:
                cleanup_errors.append("short cgroup kill write")
        except OSError:
            cleanup_errors.append("owned cgroup kill failed")
        finally:
            os.close(kill_fd)
        for child in children:
            try:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=10)
            except (OSError, subprocess.TimeoutExpired):
                cleanup_errors.append("tracked child exit could not be confirmed")
        deadline = time.monotonic() + 10
        try:
            require_directory_identity(root, identity)
            while not group_is_empty(root_fd):
                if time.monotonic() >= deadline:
                    raise TimeoutError("owned cgroup remained populated")
                time.sleep(0.02)
            remove_empty_cgroup_children(root_fd)
            require_directory_identity(root, identity)
            root.rmdir()
        except (OSError, TimeoutError):
            cleanup_errors.append("owned cgroup cleanup could not be confirmed")
        finally:
            for fd in group_fds:
                os.close(fd)
            os.close(root_fd)
        if cleanup_errors:
            raise RuntimeError("; ".join(cleanup_errors) + f"; broker artifacts retained at {temporary}")
        if temporary is not None:
            shutil.rmtree(temporary)  # Exact private directory created by this run, after confirmed teardown.


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--child":
        child_exec()
    else:
        run()
