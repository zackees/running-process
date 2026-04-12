#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import atexit
import faulthandler
import os
import queue
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path

from ci.env import clean_env

ROOT = Path(__file__).resolve().parent.parent

_STACK_DUMP_TIMEOUT_ENV = "RUN_LOGGED_STACK_DUMP_SECONDS"
_DEFAULT_STACK_DUMP_TIMEOUT_SECONDS = 180.0


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
        reader = threading.Thread(
            target=_stdout_reader,
            args=(process.stdout, lines),
            name="run-logged-stdout-reader",
            daemon=True,
        )
        last_output = time.monotonic()

        def _get_last_output() -> float:
            return last_output

        watchdog = threading.Thread(
            target=_watchdog,
            kwargs={
                "stop_event": stop_watchdog,
                "handle": handle,
                "command": command,
                "pid": process.pid,
                "stack_dump_timeout": stack_dump_timeout,
                "get_last_output": _get_last_output,
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
                last_output = time.monotonic()
            reader.join(timeout=1.0)
            return process.wait()
        finally:
            _disarm_watchdog()
            atexit.unregister(_disarm_watchdog)


if __name__ == "__main__":
    sys.exit(main())
