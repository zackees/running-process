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
        raise RuntimeError(f"{binary.name} {' '.join(args)} exited {result.returncode}")
    return result.stdout


def exercise_daemon_cli(binary: Path, *, env: dict[str, str]) -> None:
    """Exercise daemon-native inspection and session administration commands."""
    _run(binary, "ping", env=env)
    _run(binary, "status", env=env)
    _run(binary, "list", env=env)
    _run(binary, "list", "--json", env=env)
    _run(binary, "list", "--originator", "coverage-smoke", env=env)
    _run(binary, "kill-zombies", "--dry-run", env=env)
    _run(binary, "tree", str(os.getpid()), env=env)
    _run(binary, "sessions", "list", env=env)
    _run(binary, "sessions", "list", "--pty", env=env)
    _run(binary, "sessions", "list", "--pipe", env=env)
    _run(binary, "sessions", "purge", env=env)
    _run(
        binary,
        "sessions",
        "kill-older",
        "--older-than",
        "999d",
        "--originator",
        "coverage-smoke",
        env=env,
    )


def exercise_cleanup(binary: Path) -> None:
    """Exercise read-only and dry-run cleanup commands in an empty registry."""
    with tempfile.TemporaryDirectory(prefix="running-process-cleanup-coverage-") as raw:
        registry = Path(raw) / "registry"
        env = os.environ.copy()
        _run(binary, "--registry-dir", str(registry), "list", env=env)
        _run(binary, "--registry-dir", str(registry), "list", "--json", env=env)
        _run(
            binary,
            "--registry-dir",
            str(registry),
            "prune",
            "--dormant-after",
            "1d",
            "--json",
            env=env,
        )
        _run(
            binary,
            "--registry-dir",
            str(registry),
            "uninstall",
            "missing-service",
            "--json",
            env=env,
        )
        _run(binary, "--registry-dir", str(registry), "instances", env=env)
        _run(
            binary,
            "--registry-dir",
            str(registry),
            "instances",
            "--status",
            "--json",
            env=env,
        )


def exercise_brokers(v1_binary: Path, v2_binary: Path) -> None:
    """Exercise the brokers' bounded operator and configuration surfaces."""
    env = os.environ.copy()
    env["RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED"] = "1"
    for args in (
        ("--version",),
        ("--help",),
        ("status",),
        ("status", "--json"),
        ("dump",),
        ("list-instances",),
        ("healthz",),
        ("readyz",),
        ("backend-health", "coverage-service"),
        ("config",),
        ("diagnose", "--output", "coverage-bundle.tar.gz"),
        ("metrics",),
    ):
        _run(v1_binary, *args, env=env)

    with tempfile.TemporaryDirectory(prefix="running-process-servicedef-coverage-") as raw:
        _run(
            v1_binary,
            "servicedef",
            "install",
            "--service",
            "coverage-service",
            "--binary-path",
            str(v1_binary),
            "--isolation",
            "explicit",
            "--explicit-instance",
            "coverage",
            "--min-version",
            "1.0.0",
            "--allow-version",
            "1.0.0",
            "--allow-version",
            "2.0.0",
            "--service-def-dir",
            raw,
            "--json",
            env=env,
        )

    _run(v2_binary, "--no-bind", "--program", "coverage-broker", env=env)


def exercise_runpm(binary: Path, daemon_binary: Path | None = None) -> None:
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
            "import os,time; print('coverage-child:' + os.environ['RP_COVERAGE']); time.sleep(20)"
        )
        try:
            _run(binary, "kill", env=env)
            _run(binary, "--start-daemon", env=env)
            _run(binary, "ping", env=env)
            if daemon_binary is not None:
                exercise_daemon_cli(daemon_binary, env=env)
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
    daemon_binary = args.bin_dir / "running-process-daemon"
    cleanup_binary = args.bin_dir / "running-process-cleanup"
    broker_v1_binary = args.bin_dir / "running-process-broker-v1"
    broker_v2_binary = args.bin_dir / "running-process-broker-v2"
    for required in (
        daemon_binary,
        cleanup_binary,
        broker_v1_binary,
        broker_v2_binary,
    ):
        if not required.is_file():
            raise RuntimeError(f"instrumented binary not found at {required}")
    exercise_cleanup(cleanup_binary)
    exercise_brokers(broker_v1_binary, broker_v2_binary)
    exercise_runpm(binary, daemon_binary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
