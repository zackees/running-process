from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

PYTHON_PRODUCTION_ROOT = ROOT / "src"
# Scan all Rust source under both crates/ and testbins/. The testbin
# scan exists specifically because issue #115 was a bare
# `Command::spawn` in testbins/spawner/src/main.rs that survived the
# earlier sanitization round precisely because testbins weren't checked.
RUST_SOURCE_ROOTS = (ROOT / "crates", ROOT / "testbins")

ALLOWED_RUST_COMMAND_NEW = {
    Path("crates/running-process-core/src/lib.rs"),
    Path("crates/running-process-core/src/containment.rs"),
    # Inline tests module for running-process-core lib root.
    Path("crates/running-process-core/src/tests.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Python-bindings containment mirror of core's containment.rs.
    Path("crates/running-process-py/src/containment.rs"),
    # Python-bindings inline test fixtures.
    Path("crates/running-process-py/src/tests/pty_process.rs"),
    # Client crate: auto-starts the daemon process when connecting.
    Path("crates/running-process-client/src/client.rs"),
    # Daemon crate: process management for daemonize, shadow-copy, and auto-start
    Path("crates/running-process-daemon/src/client.rs"),
    Path("crates/running-process-daemon/src/handlers.rs"),
    Path("crates/running-process-daemon/src/platform/windows.rs"),
    Path("crates/running-process-daemon/src/shadow.rs"),
    # Daemon trampoline: reads sidecar JSON and spawns the target command
    Path("crates/daemon-trampoline/src/main.rs"),
    # Client crate: spawns daemon as detached background process
    Path("crates/running-process-client/src/client.rs"),
    # Test-only watchdog crate (publish=false, dev-dep only) — invokes
    # procdump.exe via Command::new when the watchdog fires.
    Path("crates/test-watchdog/src/lib.rs"),
    # Testbins (test fixtures): builds Command values to hand to the
    # sanitized spawn surface (or, on Unix, to bare std::Command::spawn
    # because our sanitized spawn calls setpgid in the child, which
    # would break the test's killpg-based containment on macOS).
    Path("testbins/spawner/src/main.rs"),
    Path("testbins/dies_after_spawn/src/main.rs"),
}

ALLOWED_RUST_SPAWN = {
    Path("crates/running-process-core/src/lib.rs"),
    Path("crates/running-process-core/src/containment.rs"),
    # Inline tests module for running-process-core lib root.
    Path("crates/running-process-core/src/tests.rs"),
    # Two-mode spawn surface: `spawn` (contained, sanitized handles, caller stdio)
    # and `spawn_daemon` (detached, NUL stdio, sanitized handles). See #110, #113.
    Path("crates/running-process-core/src/spawn.rs"),
    # Unix backing impl for `spawn`/`spawn_daemon`: drives bare
    # `Command::spawn()` after applying setpgid/setsid + fd hygiene
    # via `pre_exec`. Windows impl uses CreateProcessW directly so it
    # doesn't trigger this lint.
    Path("crates/running-process-core/src/spawn_imp_unix.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Python-bindings containment mirror of core's containment.rs.
    Path("crates/running-process-py/src/containment.rs"),
    # Python-bindings inline test fixtures.
    Path("crates/running-process-py/src/tests/pty_process.rs"),
    # Client crate: auto-starts the daemon process when connecting.
    Path("crates/running-process-client/src/client.rs"),
    # Daemon crate: process management for daemonize, shadow-copy, and auto-start
    Path("crates/running-process-daemon/src/client.rs"),
    Path("crates/running-process-daemon/src/handlers.rs"),
    Path("crates/running-process-daemon/src/platform/windows.rs"),
    Path("crates/running-process-daemon/src/shadow.rs"),
    # Daemon trampoline: reads sidecar JSON and spawns the target command
    Path("crates/daemon-trampoline/src/main.rs"),
    # Client crate: spawns daemon as detached background process
    Path("crates/running-process-client/src/client.rs"),
    # Test-only watchdog crate: spawns procdump.exe with .output()
    # which is .spawn() + wait under the hood.
    Path("crates/test-watchdog/src/lib.rs"),
    # Testbins: bare std::Command::spawn on Unix only (see comment in
    # testbins/spawner — sanitized spawn isn't usable there because of
    # the setpgid-vs-killpg interaction the containment test relies on).
    Path("testbins/spawner/src/main.rs"),
}

ALLOWED_PORTABLE_PTY = {
    Path("crates/running-process-py/src/lib.rs"),
    # PTY module moved to core crate
    Path("crates/running-process-core/src/pty/mod.rs"),
    # Native PTY process impl extracted from pty/mod.rs.
    Path("crates/running-process-core/src/pty/native_pty_process.rs"),
}

# `ChildStdin::from` / `ChildStdout::from` / `ChildStderr::from` consumes a
# raw OS handle. Rust's `ChildPipe::read`/`write` on Windows uses
# `alertable_io_internal` (overlapped I/O + alertable `SleepEx`); pairing
# that against a synchronous handle silently drops every write after the
# first — exactly issue #115. The typed `OverlappedHandle::into_child_*`
# wrappers in spawn_imp_windows.rs are the ONLY approved way to do this
# conversion, and they bake in the FILE_FLAG_OVERLAPPED guarantee.
ALLOWED_RUST_CHILD_PIPE_FROM = {
    Path("crates/running-process-core/src/spawn_imp_windows.rs"),
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
    portable_pty_pattern = re.compile(r"\bportable_pty\b")
    # Banned outright across the workspace: CreatePipe creates
    # synchronous-only anonymous pipes. Wrapping the parent end in a Rust
    # ChildStd* (whose read uses alertable_io_internal / overlapped I/O)
    # silently drops every write after the first. Use the typed
    # `create_pipe_pair` helper in spawn_imp_windows.rs instead.
    # See issue #115.
    create_pipe_pattern = re.compile(r"\bCreatePipe\s*\(")
    # Bypass detector for the typed-pipe API: only the typed
    # `OverlappedHandle::into_child_*` wrappers in spawn_imp_windows.rs
    # should construct a `ChildStd*` from a raw handle.
    child_pipe_from_pattern = re.compile(r"\bChild(?:Stdin|Stdout|Stderr)::from\s*\(")

    for root in RUST_SOURCE_ROOTS:
        if not root.exists():
            continue
        for path in _iter_files(root, ".rs"):
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

            # CreatePipe is banned workspace-wide. No allowlist —
            # `create_pipe_pair` in spawn_imp_windows.rs uses
            # CreateNamedPipeW + CreateFileW instead and is the only
            # sanctioned construction.
            create_pipe_hits = _find_matches(path, create_pipe_pattern)
            if create_pipe_hits:
                failures.extend(
                    _format_hits(
                        path,
                        create_pipe_hits,
                        (
                            "CreatePipe is banned: it returns sync-only handles "
                            "incompatible with Rust's ChildStd* reader; use "
                            "create_pipe_pair (spawn_imp_windows.rs). See #115."
                        ),
                    )
                )

            child_pipe_from_hits = _find_matches(path, child_pipe_from_pattern)
            if child_pipe_from_hits and rel not in ALLOWED_RUST_CHILD_PIPE_FROM:
                failures.extend(
                    _format_hits(
                        path,
                        child_pipe_from_hits,
                        (
                            "ChildStd* construction from a raw handle bypasses the "
                            "typed OverlappedHandle API; route through "
                            "OverlappedHandle::into_child_* in spawn_imp_windows.rs. "
                            "See #115."
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
