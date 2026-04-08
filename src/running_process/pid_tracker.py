from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from running_process._native import list_tracked_processes as _native_list_tracked_processes
from running_process._native import (
    native_cleanup_tracked_processes as _native_cleanup_tracked_processes,
)
from running_process._native import native_is_same_process as _native_is_same_process
from running_process._native import native_register_process as _native_register_process
from running_process._native import native_unregister_process as _native_unregister_process
from running_process._native import tracked_pid_db_path_py as _native_tracked_pid_db_path
from running_process.command_render import list2cmdline

CREATE_TIME_TOLERANCE_SECONDS = 1.0
KILL_TIMEOUT_SECONDS = 3.0


@dataclass(frozen=True, slots=True)
class TrackedProcess:
    pid: int
    created_at: float
    kind: str
    command: str
    cwd: str | None


def tracked_pid_db_path() -> Path:
    return Path(_native_tracked_pid_db_path())


def _command_text(command: str | list[str]) -> str:
    if isinstance(command, str):
        return command
    return list2cmdline(command)


def register_process(
    pid: int | None, *, kind: str, command: str | list[str], cwd: str | Path | None
) -> None:
    if pid is None:
        return
    _native_register_process(
        int(pid),
        kind,
        _command_text(command),
        str(cwd) if cwd is not None else None,
    )


def unregister_process(pid: int | None) -> None:
    if pid is None:
        return
    _native_unregister_process(int(pid))


def list_tracked_processes() -> list[TrackedProcess]:
    return [TrackedProcess(*row) for row in _native_list_tracked_processes()]


def is_same_process(entry: TrackedProcess) -> bool:
    return _native_is_same_process(
        entry.pid,
        entry.created_at,
        CREATE_TIME_TOLERANCE_SECONDS,
    )


def cleanup_tracked_processes() -> list[TrackedProcess]:
    return [
        TrackedProcess(*row)
        for row in _native_cleanup_tracked_processes(
            CREATE_TIME_TOLERANCE_SECONDS,
            KILL_TIMEOUT_SECONDS,
        )
    ]
