#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import atexit
import faulthandler
import json
import os
import queue
import shlex
import subprocess
import sys
import threading
import time
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from ci.env import clean_env

ROOT = Path(__file__).resolve().parent.parent

_STACK_DUMP_TIMEOUT_ENV = "RUN_LOGGED_STACK_DUMP_SECONDS"
_DEFAULT_STACK_DUMP_TIMEOUT_SECONDS = 180.0
_TAIL_LINE_LIMIT = 20


@dataclass
class RunAnalytics:
    command: list[str]
    log_path: str
    started_at_utc: str
    line_count: int = 0
    byte_count: int = 0
    idle_dump_count: int = 0
    max_idle_seconds: float = 0.0
    first_output_seconds: float | None = None
    tail_lines: deque[str] = field(
        default_factory=lambda: deque(maxlen=_TAIL_LINE_LIMIT)
    )


def _write_console_line(line: str) -> None:
    try:
        sys.stdout.write(line)
    except UnicodeEncodeError:
        encoding = sys.stdout.encoding or "utf-8"
        rendered = line.encode(encoding, errors="replace")
        if hasattr(sys.stdout, "buffer"):
            sys.stdout.buffer.write(rendered)
        else:
            sys.stdout.write(rendered.decode(encoding, errors="replace"))
    sys.stdout.flush()


def _stack_dump_timeout_seconds() -> float:
    value = os.environ.get(_STACK_DUMP_TIMEOUT_ENV)
    if value is None:
        return _DEFAULT_STACK_DUMP_TIMEOUT_SECONDS
    try:
        parsed = float(value)
    except ValueError:
        return _DEFAULT_STACK_DUMP_TIMEOUT_SECONDS
    return max(5.0, parsed)


def _child_env() -> dict[str, str]:
    env = clean_env()
    env.setdefault("PYTHONFAULTHANDLER", "1")
    return env


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _analytics_path(log_path: Path) -> Path:
    return log_path.with_name(f"{log_path.name}.analytics.json")


def _record_output_line(
    analytics: RunAnalytics,
    *,
    line: str,
    started_at: float,
    last_output_at: float,
    now: float,
) -> float:
    analytics.max_idle_seconds = max(analytics.max_idle_seconds, max(0.0, now - last_output_at))
    if analytics.first_output_seconds is None:
        analytics.first_output_seconds = max(0.0, now - started_at)
    analytics.line_count += 1
    analytics.byte_count += len(line.encode("utf-8", errors="replace"))
    analytics.tail_lines.append(line.rstrip("\r\n"))
    return now


def _build_failure_analytics(
    analytics: RunAnalytics,
    *,
    returncode: int,
    started_at: float,
    last_output_at: float,
) -> dict[str, object]:
    finished_at = _utc_now_iso()
    duration_seconds = max(0.0, time.monotonic() - started_at)
    last_idle_seconds = max(0.0, time.monotonic() - last_output_at)
    max_idle_seconds = max(analytics.max_idle_seconds, last_idle_seconds)
    return {
        "command": analytics.command,
        "command_pretty": shlex.join(analytics.command),
        "log_path": analytics.log_path,
        "started_at_utc": analytics.started_at_utc,
        "finished_at_utc": finished_at,
        "duration_seconds": round(duration_seconds, 3),
        "exit_code": int(returncode),
        "line_count": analytics.line_count,
        "byte_count": analytics.byte_count,
        "idle_dump_count": analytics.idle_dump_count,
        "first_output_seconds": (
            None
            if analytics.first_output_seconds is None
            else round(analytics.first_output_seconds, 3)
        ),
        "last_idle_seconds": round(last_idle_seconds, 3),
        "max_idle_seconds": round(max_idle_seconds, 3),
        "tail_lines": list(analytics.tail_lines),
    }


def _render_failure_summary(report: dict[str, object]) -> str:
    lines = [
        "\n[run_logged] failure analytics",
        f"  command: {report['command_pretty']}",
        f"  log_path: {report['log_path']}",
        f"  exit_code: {report['exit_code']}",
        f"  duration_seconds: {report['duration_seconds']}",
        f"  first_output_seconds: {report['first_output_seconds']}",
        f"  last_idle_seconds: {report['last_idle_seconds']}",
        f"  max_idle_seconds: {report['max_idle_seconds']}",
        f"  idle_dump_count: {report['idle_dump_count']}",
        f"  line_count: {report['line_count']}",
        f"  byte_count: {report['byte_count']}",
        "  tail_lines:",
    ]
    tail_lines = report["tail_lines"]
    assert isinstance(tail_lines, list)
    if tail_lines:
        lines.extend(f"    {line}" for line in tail_lines)
    else:
        lines.append("    <no output captured>")
    return "\n".join(lines) + "\n"


def _write_failure_analytics(log_path: Path, report: dict[str, object]) -> Path:
    analytics_path = _analytics_path(log_path)
    analytics_path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return analytics_path


def _dump_thread_stacks(handle, *, command: list[str], pid: int | None, idle_for: float) -> None:
    header = (
        f"\n[run_logged] no output for {idle_for:.1f}s; "
        f"dumping thread stacks for pid={pid} command={command!r}\n"
    )
    for stream in (sys.stderr, handle):
        stream.write(header)
        stream.flush()
        faulthandler.dump_traceback(file=stream, all_threads=True)
        stream.flush()


def _stdout_reader(stdout, lines: queue.Queue[str | None]) -> None:
    try:
        for line in stdout:
            lines.put(line)
    finally:
        stdout.close()
        lines.put(None)


def _watchdog(
    *,
    stop_event: threading.Event,
    handle,
    command: list[str],
    pid: int | None,
    stack_dump_timeout: float,
    get_last_output: Callable[[], float],
    on_stack_dump: Callable[[], None],
) -> None:
    last_dump_at: float | None = None
    while not stop_event.wait(1.0):
        idle_for = time.monotonic() - get_last_output()
        if idle_for < stack_dump_timeout:
            last_dump_at = None
            continue
        if last_dump_at is not None and (time.monotonic() - last_dump_at) < stack_dump_timeout:
            continue
        _dump_thread_stacks(handle, command=command, pid=pid, idle_for=idle_for)
        on_stack_dump()
        last_dump_at = time.monotonic()


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        print("usage: run_logged.py <log-path> -- <command...>", file=sys.stderr)
        return 2

    log_path = Path(sys.argv[1])
    command = sys.argv[3:]
    log_path.parent.mkdir(parents=True, exist_ok=True)
    stack_dump_timeout = _stack_dump_timeout_seconds()

    with log_path.open("w", encoding="utf-8", errors="replace") as handle:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=_child_env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        assert process.stdout is not None
        lines: queue.Queue[str | None] = queue.Queue()
        stop_watchdog = threading.Event()
        analytics_lock = threading.Lock()
        started_at = time.monotonic()
        analytics = RunAnalytics(
            command=list(command),
            log_path=str(log_path),
            started_at_utc=_utc_now_iso(),
        )
        reader = threading.Thread(
            target=_stdout_reader,
            args=(process.stdout, lines),
            name="run-logged-stdout-reader",
            daemon=True,
        )
        last_output = started_at

        def _get_last_output() -> float:
            return last_output

        def _record_stack_dump() -> None:
            with analytics_lock:
                analytics.idle_dump_count += 1

        watchdog = threading.Thread(
            target=_watchdog,
            kwargs={
                "stop_event": stop_watchdog,
                "handle": handle,
                "command": command,
                "pid": process.pid,
                "stack_dump_timeout": stack_dump_timeout,
                "get_last_output": _get_last_output,
                "on_stack_dump": _record_stack_dump,
            },
            name="run-logged-watchdog",
            daemon=True,
        )

        def _disarm_watchdog() -> None:
            stop_watchdog.set()

        atexit.register(_disarm_watchdog)
        reader.start()
        watchdog.start()

        try:
            while True:
                try:
                    line = lines.get(timeout=1.0)
                except queue.Empty:
                    if process.poll() is not None and not reader.is_alive():
                        break
                    continue

                if line is None:
                    break

                _write_console_line(line)
                handle.write(line)
                handle.flush()
                now = time.monotonic()
                with analytics_lock:
                    last_output = _record_output_line(
                        analytics,
                        line=line,
                        started_at=started_at,
                        last_output_at=last_output,
                        now=now,
                    )
            reader.join(timeout=1.0)
            returncode = process.wait()
            if returncode != 0:
                with analytics_lock:
                    report = _build_failure_analytics(
                        analytics,
                        returncode=returncode,
                        started_at=started_at,
                        last_output_at=last_output,
                    )
                analytics_path = _write_failure_analytics(log_path, report)
                summary = _render_failure_summary(report)
                for stream in (sys.stderr, handle):
                    stream.write(summary)
                    stream.write(f"[run_logged] analytics written to {analytics_path}\n")
                    stream.flush()
            return returncode
        finally:
            _disarm_watchdog()
            atexit.unregister(_disarm_watchdog)


if __name__ == "__main__":
    sys.exit(main())
