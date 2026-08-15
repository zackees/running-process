from __future__ import annotations

import asyncio
import math
import signal as signal_module
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any, Literal

from running_process.exit_status import ExitStatus, classify_exit_status


class ObservationPolicy(str, Enum):
    NON_INVASIVE = "non_invasive"
    ALLOW_TRACING = "allow_tracing"
    REQUIRE_EXACT = "require_exact"


class ObservationGrade(str, Enum):
    EXACT_TRACE = "exact_trace"
    EXACT_EVENT = "exact_event"
    KERNEL_NOTIFICATION = "kernel_notification"
    KERNEL_HINT_RECONCILED = "kernel_hint_reconciled"
    SNAPSHOT_INFERRED = "snapshot_inferred"


class StackCapture(str, Enum):
    ORIGIN_PREFERRED = "origin_preferred"
    ORIGIN_REQUIRED = "origin_required"
    OWNER_ALL_THREADS = "owner_all_threads"


class CaptureSource(str, Enum):
    REMOTE_SPAWNING_THREAD = "remote_spawning_thread"
    MANAGED_SPAWN_BOUNDARY = "managed_spawn_boundary"
    OWNER_EVENT_TIME_SNAPSHOT = "owner_event_time_snapshot"
    NONE = "none"


class ProcessEventKind(str, Enum):
    SPAWN = "spawn"
    EXEC = "exec"
    EXIT = "exit"
    LOSS = "loss"


class ProcessObservationUnavailableError(RuntimeError):
    pass


class ProcessWatchConfigurationError(ValueError):
    pass


@dataclass(frozen=True, slots=True)
class StackDump:
    capture: StackCapture = StackCapture.ORIGIN_PREFERRED
    directory: Path | None = None
    symbolize: Literal["deferred", "immediate"] = "deferred"

    def __post_init__(self) -> None:
        if self.symbolize not in ("deferred", "immediate"):
            raise ProcessWatchConfigurationError(
                "symbolize must be 'deferred' or 'immediate'"
            )


@dataclass(frozen=True, slots=True)
class ProcessWatch:
    _kind: str
    basename: str | None = None
    path: Path | None = None
    code: int | None = None
    signal: int | None = None
    dump: StackDump | None = None
    limit: int | None = 1
    cooldown_seconds: float = 0.0
    label: str = ""

    def __post_init__(self) -> None:
        if self.limit is not None and (not isinstance(self.limit, int) or self.limit < 1):
            raise ProcessWatchConfigurationError("limit must be positive or None")
        if not math.isfinite(self.cooldown_seconds) or self.cooldown_seconds < 0:
            raise ProcessWatchConfigurationError(
                "cooldown_seconds must be a finite non-negative number"
            )
        if not self.label.strip():
            raise ProcessWatchConfigurationError("label must not be empty")
        if self.basename == "":
            raise ProcessWatchConfigurationError("basename must not be empty")
        if self.basename is not None and self.path is not None:
            raise ProcessWatchConfigurationError("provide basename or path, not both")
        if self.code is not None and self.signal is not None:
            raise ProcessWatchConfigurationError("provide code or signal, not both")

    @classmethod
    def on_exec(
        cls,
        basename: str | None = None,
        *,
        path: Path | None = None,
        dump: StackDump | None = None,
        limit: int | None = 1,
        cooldown_seconds: float = 0.0,
        label: str | None = None,
    ) -> ProcessWatch:
        return cls(
            "exec",
            basename=basename,
            path=path,
            dump=dump,
            limit=limit,
            cooldown_seconds=cooldown_seconds,
            label=label or f"exec:{basename or path or '*'}",
        )

    @classmethod
    def on_spawn(
        cls,
        *,
        dump: StackDump | None = None,
        limit: int | None = 1,
        cooldown_seconds: float = 0.0,
        label: str | None = None,
    ) -> ProcessWatch:
        return cls(
            "spawn",
            dump=dump,
            limit=limit,
            cooldown_seconds=cooldown_seconds,
            label=label or "spawn:*",
        )

    @classmethod
    def on_exit(
        cls,
        code: int | None = None,
        *,
        signal: int | signal_module.Signals | None = None,
        basename: str | None = None,
        dump: StackDump | None = None,
        limit: int | None = 1,
        cooldown_seconds: float = 0.0,
        label: str | None = None,
    ) -> ProcessWatch:
        signal_number = int(signal) if signal is not None else None
        selector = f"code={code}" if code is not None else f"signal={signal_number}"
        if code is None and signal is None:
            selector = "*"
        return cls(
            "exit",
            basename=basename,
            code=code,
            signal=signal_number,
            dump=dump,
            limit=limit,
            cooldown_seconds=cooldown_seconds,
            label=label or f"exit:{basename or '*'}:{selector}",
        )

    @classmethod
    def on_failure(
        cls,
        *,
        basename: str | None = None,
        dump: StackDump | None = None,
        limit: int | None = 1,
        cooldown_seconds: float = 0.0,
        label: str | None = None,
    ) -> ProcessWatch:
        return cls(
            "failure",
            basename=basename,
            dump=dump,
            limit=limit,
            cooldown_seconds=cooldown_seconds,
            label=label or f"failure:{basename or '*'}",
        )

    def _native(self) -> dict[str, Any]:
        return {
            "kind": self._kind,
            "basename": self.basename,
            "path": str(self.path) if self.path is not None else None,
            "code": self.code,
            "signal": self.signal,
            "limit": self.limit,
            "cooldown_seconds": self.cooldown_seconds,
            "label": self.label,
            "dump_capture": self.dump.capture.value if self.dump else None,
            "dump_directory": (
                str(self.dump.directory)
                if self.dump is not None and self.dump.directory is not None
                else None
            ),
            "dump_symbolize": self.dump.symbolize if self.dump else None,
        }


@dataclass(frozen=True, slots=True)
class ProcessIdentity:
    pid: int
    start_key: int | str | None


@dataclass(frozen=True, slots=True)
class ProcessEvent:
    kind: ProcessEventKind
    process: ProcessIdentity
    parent: ProcessIdentity | None
    timestamp: datetime
    executable: Path | None
    argv: tuple[str, ...] | None
    exit_status: ExitStatus | None
    raw_exit_status: int | None
    backend: str
    observation_grade: ObservationGrade
    coverage_complete: bool
    loss_detected: bool


@dataclass(frozen=True, slots=True)
class DumpResult:
    capture_source: CaptureSource
    artifacts: tuple[Path, ...]
    symbolized: bool
    error: str | None


@dataclass(frozen=True, slots=True)
class ProcessWatchMatch:
    sequence: int
    watch: ProcessWatch
    event: ProcessEvent
    dump: DumpResult | None


@dataclass(frozen=True, slots=True)
class ProcessWatchGap:
    first_missing: int
    last_missing: int


@dataclass(frozen=True, slots=True)
class ProcessObservationCapabilities:
    exact_available: bool
    exact_backend: str
    reason: str


@dataclass(frozen=True, slots=True)
class ProcessObservation:
    backend: str
    observation_grade: ObservationGrade
    fallback_reason: str | None


def _exit_status(raw: dict[str, Any]) -> ExitStatus | None:
    code = raw.get("exit_code")
    signal_number = raw.get("signal")
    if code is None and signal_number is None:
        return None
    logical = (
        -signal_number
        if signal_number is not None and sys.platform != "win32"
        else int(code or 0)
    )
    return classify_exit_status(logical, set())


def _match_from_native(
    raw: dict[str, Any], watches: dict[str, ProcessWatch]
) -> ProcessWatchMatch:
    event = raw["event"]
    parent_pid = event.get("parent_pid")
    dump_raw = raw.get("dump")
    return ProcessWatchMatch(
        sequence=raw["sequence"],
        watch=watches[raw["watch_label"]],
        event=ProcessEvent(
            kind=ProcessEventKind(event["kind"]),
            process=ProcessIdentity(event["pid"], event.get("start_key")),
            parent=(
                ProcessIdentity(parent_pid, event.get("parent_start_key"))
                if parent_pid is not None
                else None
            ),
            timestamp=datetime.fromtimestamp(event["timestamp"], timezone.utc),
            executable=Path(event["executable"]) if event.get("executable") else None,
            argv=tuple(event["argv"]) if event.get("argv") is not None else None,
            exit_status=_exit_status(event),
            raw_exit_status=event.get("raw_exit_status"),
            backend=event["backend"],
            observation_grade=ObservationGrade(event["observation_grade"]),
            coverage_complete=event["coverage_complete"],
            loss_detected=event["loss_detected"],
        ),
        dump=(
            DumpResult(
                capture_source=CaptureSource(dump_raw["capture_source"]),
                artifacts=tuple(Path(path) for path in dump_raw["artifacts"]),
                symbolized=dump_raw["symbolized"],
                error=dump_raw.get("error"),
            )
            if dump_raw is not None
            else None
        ),
    )


class ProcessWatchCursor:
    def __init__(self, native_process: Any, watches: dict[str, ProcessWatch]) -> None:
        self._native_process = native_process
        self._watches = watches
        self._cursor_id = native_process.open_process_watch_cursor()
        if self._cursor_id is None:
            raise RuntimeError("this process has no process watches")

    def read_next(
        self, timeout: float | None = None
    ) -> ProcessWatchMatch | ProcessWatchGap | None:
        raw = self._native_process.take_process_watch_match(self._cursor_id, timeout)
        if raw["type"] == "eof":
            return None
        if raw["type"] == "timeout":
            raise TimeoutError("no process-watch match available before timeout")
        if raw["type"] == "gap":
            return ProcessWatchGap(raw["first_missing"], raw["last_missing"])
        return _match_from_native(raw, self._watches)

    def __iter__(self) -> ProcessWatchCursor:
        return self

    def __next__(self) -> ProcessWatchMatch | ProcessWatchGap:
        item = self.read_next()
        if item is None:
            raise StopIteration
        return item


class AsyncProcessWatchCursor:
    def __init__(self, cursor: ProcessWatchCursor) -> None:
        self._cursor = cursor

    async def read_next(self) -> ProcessWatchMatch | ProcessWatchGap | None:
        return await asyncio.to_thread(self._cursor.read_next)

    def __aiter__(self) -> AsyncProcessWatchCursor:
        return self

    async def __anext__(self) -> ProcessWatchMatch | ProcessWatchGap:
        item = await self.read_next()
        if item is None:
            raise StopAsyncIteration
        return item
