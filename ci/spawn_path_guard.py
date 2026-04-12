from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

PYTHON_PRODUCTION_ROOT = ROOT / "src"
RUST_SOURCE_ROOT = ROOT / "crates"

ALLOWED_RUST_COMMAND_NEW = {
    Path("crates/running-process-core/src/lib.rs"),
    Path("crates/running-process-core/src/containment.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Daemon crate: process management for daemonize, shadow-copy, and auto-start
    Path("crates/running-process-daemon/src/client.rs"),
    Path("crates/running-process-daemon/src/platform/windows.rs"),
    Path("crates/running-process-daemon/src/shadow.rs"),
    # Daemon trampoline: reads sidecar JSON and spawns the target command
    Path("crates/daemon-trampoline/src/main.rs"),
}

ALLOWED_RUST_SPAWN = {
    Path("crates/running-process-core/src/lib.rs"),
    Path("crates/running-process-core/src/containment.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Daemon crate: process management for daemonize, shadow-copy, and auto-start
    Path("crates/running-process-daemon/src/client.rs"),
    Path("crates/running-process-daemon/src/platform/windows.rs"),
    Path("crates/running-process-daemon/src/shadow.rs"),
    # Daemon trampoline: reads sidecar JSON and spawns the target command
    Path("crates/daemon-trampoline/src/main.rs"),
}

ALLOWED_PORTABLE_PTY = {
    Path("crates/running-process-py/src/lib.rs"),
}

ALLOWED_PYTHON_POPEN = {
    Path("src/running_process/cli.py"),
    # Daemon spawner: subprocess.Popen to launch the trampoline binary
    Path("src/running_process/daemon.py"),
}


def _iter_files(root: Path, suffix: str) -> list[Path]:
    return sorted(path for path in root.rglob(f"*{suffix}") if path.is_file())


def _relative(path: Path) -> Path:
    return path.relative_to(ROOT)


def _find_matches(path: Path, pattern: re.Pattern[str]) -> list[int]:
    lines: list[int] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if pattern.search(line):
            lines.append(line_number)
    return lines


def _format_hits(path: Path, lines: list[int], message: str) -> list[str]:
    rel = _relative(path)
    return [f"{rel}:{line}: {message}" for line in lines]


def check_python_spawn_sites() -> list[str]:
    failures: list[str] = []
    popen_pattern = re.compile(r"\bsubprocess\.Popen\s*\(")
    for path in _iter_files(PYTHON_PRODUCTION_ROOT, ".py"):
        hits = _find_matches(path, popen_pattern)
        if hits and _relative(path) not in ALLOWED_PYTHON_POPEN:
            failures.extend(
                _format_hits(
                    path,
                    hits,
                    "raw subprocess.Popen in production code bypasses native lifecycle enforcement",
                )
            )
    return failures


def check_rust_spawn_sites() -> list[str]:
    failures: list[str] = []
    command_new_pattern = re.compile(r"\bCommand::new\s*\(")
    spawn_pattern = re.compile(r"\.spawn\s*\(")
    portable_pty_pattern = re.compile(r"\bportable_pty\b|\bspawn_command\s*\(")

    for path in _iter_files(RUST_SOURCE_ROOT, ".rs"):
        rel = _relative(path)
        if "src" not in rel.parts:
            continue
        command_new_hits = _find_matches(path, command_new_pattern)
        if command_new_hits and rel not in ALLOWED_RUST_COMMAND_NEW:
            failures.extend(
                _format_hits(
                    path,
                    command_new_hits,
                    "Command::new outside the native spawn layer requires review and allowlisting",
                )
            )

        spawn_hits = _find_matches(path, spawn_pattern)
        if spawn_hits and rel not in ALLOWED_RUST_SPAWN:
            failures.extend(
                _format_hits(
                    path,
                    spawn_hits,
                    "spawn() outside the native spawn layer requires review and allowlisting",
                )
            )

        portable_pty_hits = _find_matches(path, portable_pty_pattern)
        if portable_pty_hits and rel not in ALLOWED_PORTABLE_PTY:
            failures.extend(
                _format_hits(
                    path,
                    portable_pty_hits,
                    (
                        "portable_pty usage outside the PTY native layer "
                        "requires review and allowlisting"
                    ),
                )
            )
    return failures


def main() -> int:
    failures = [
        *check_python_spawn_sites(),
        *check_rust_spawn_sites(),
    ]
    if not failures:
        print("spawn-path guard passed")
        return 0

    print("spawn-path guard failed:")
    for failure in failures:
        print(f"  {failure}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
