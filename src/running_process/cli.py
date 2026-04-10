from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import tempfile
from collections.abc import Sequence
from contextlib import suppress
from datetime import UTC, datetime
from pathlib import Path
from typing import BinaryIO, TextIO

from running_process.process_utils import kill_process_tree

IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"
RUNNING_PROCESS_STACK_DUMP_DIR_ENV = "RUNNING_PROCESS_STACK_DUMP_DIR"
DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE = 124
_PY_SPY_DUMP_TIMEOUT_SECONDS = 10.0
_RUST_MANGLED_SYMBOL = re.compile(r"_ZN[A-Za-z0-9_$.]+E")
_RUST_HASH_SUFFIX = re.compile(r"::h[0-9a-f]{16}$")
_SUPERVISOR_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)


def _parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a command under running-process supervision")
    parser.add_argument(
        "--timeout",
        type=float,
        default=None,
        help=(
            "Kill the child after this many seconds without stdout/stderr "
            "activity and collect stack-dump artifacts."
        ),
    )
    parser.add_argument(
        "--no-auto-stack-dumping",
        action="store_true",
        help="Disable timeout and abnormal-exit diagnostic dump collection.",
    )
    parser.add_argument(
        "--stack-dump-dir",
        type=Path,
        default=None,
        help="Directory for diagnostic dump artifacts. Defaults to logs/running-process.",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="Command to run. Use `--` before the command to avoid option parsing.",
    )
    return parser.parse_args(argv)


def _normalize_command(command: Sequence[str]) -> list[str]:
    normalized = list(command)
    if normalized and normalized[0] == "--":
        normalized = normalized[1:]
    if not normalized:
        raise SystemExit("running-process requires a command after `--`")
    return normalized


def _child_env() -> dict[str, str]:
    env = os.environ.copy()
    env[IN_RUNNING_PROCESS_ENV] = IN_RUNNING_PROCESS_VALUE
    env.setdefault("PYTHONFAULTHANDLER", "1")
    return env


def _stack_dump_dir(override: Path | None) -> Path:
    if override is not None:
        return override
    configured = os.environ.get(RUNNING_PROCESS_STACK_DUMP_DIR_ENV)
    if configured:
        return Path(configured)
    return Path.cwd() / "logs" / "running-process"


def _artifact_stem(*, reason: str, pid: int | None) -> str:
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    pid_part = str(pid) if pid is not None else "unknown"
    return f"{timestamp}-pid{pid_part}-{reason}"


def _safe_write(stream: TextIO, message: str) -> None:
    stream.write(message)
    stream.flush()


def _write_stream_bytes(stream: TextIO, data: bytes) -> None:
    buffer = getattr(stream, "buffer", None)
    if buffer is not None:
        buffer.write(data)
        stream.flush()
        return
    encoding = getattr(stream, "encoding", None) or "utf-8"
    stream.write(data.decode(encoding, errors="replace"))
    stream.flush()


def _write_dump_metadata(
    *,
    metadata_path: Path,
    reason: str,
    command: Sequence[str],
    pid: int | None,
    returncode: int | None,
    timeout_seconds: float | None,
) -> None:
    metadata = {
        "reason": reason,
        "command": list(command),
        "pid": pid,
        "returncode": returncode,
        "timeout_seconds": timeout_seconds,
        "timestamp_utc": datetime.now(UTC).isoformat(),
    }
    metadata_path.write_text(json.dumps(metadata, indent=2, sort_keys=True), encoding="utf-8")


def _run_py_spy_dump(*, pid: int | None, log_path: Path) -> bool:
    if pid is None:
        log_path.write_text("py-spy unavailable: child pid is unknown\n", encoding="utf-8")
        return False
    py_spy = shutil.which("py-spy")
    if py_spy is None:
        log_path.write_text("py-spy unavailable on PATH\n", encoding="utf-8")
        return False
    try:
        result = subprocess.run(
            [py_spy, "dump", "--pid", str(pid)],
            check=False,
            capture_output=True,
            text=True,
            timeout=_PY_SPY_DUMP_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        log_path.write_text("py-spy timed out while collecting diagnostics\n", encoding="utf-8")
        return False
    rendered = []
    rendered.append(f"$ {py_spy} dump --pid {pid}\n")
    rendered.append(f"exit_code={result.returncode}\n")
    if result.stdout:
        rendered.append("\n[stdout]\n")
        rendered.append(result.stdout)
        if not result.stdout.endswith("\n"):
            rendered.append("\n")
    if result.stderr:
        rendered.append("\n[stderr]\n")
        rendered.append(result.stderr)
        if not result.stderr.endswith("\n"):
            rendered.append("\n")
    log_path.write_text("".join(rendered), encoding="utf-8")
    return result.returncode == 0


def _native_debugger_commands(pid: int | None) -> list[list[str]]:
    if pid is None:
        return []
    commands: list[list[str]] = []
    lldb = shutil.which("lldb")
    if lldb is not None:
        commands.append(
            [
                lldb,
                "--batch",
                "-p",
                str(pid),
                "-o",
                "thread backtrace all",
                "-o",
                "detach",
                "-o",
                "quit",
            ]
        )
    gdb = shutil.which("gdb")
    if gdb is not None:
        commands.append(
            [
                gdb,
                "--batch",
                "--nx",
                "-p",
                str(pid),
            ]
        )
    return commands


def _demangle_rust_symbol(symbol: str) -> str:
    cxxfilt = shutil.which("c++filt")
    if cxxfilt is None:
        return symbol
    try:
        result = subprocess.run(
            [cxxfilt],
            check=False,
            capture_output=True,
            text=True,
            input=f"{symbol}\n",
            timeout=2.0,
        )
    except (OSError, subprocess.TimeoutExpired):
        return symbol
    if result.returncode != 0:
        return symbol
    demangled = result.stdout.strip() or symbol
    return _RUST_HASH_SUFFIX.sub("", demangled)


def _demangle_native_debugger_text(text: str) -> str:
    seen: dict[str, str] = {}

    def replace(match: re.Match[str]) -> str:
        symbol = match.group(0)
        if symbol not in seen:
            seen[symbol] = _demangle_rust_symbol(symbol)
        return seen[symbol]

    return _RUST_MANGLED_SYMBOL.sub(replace, text)


def _run_native_debugger_dump(*, pid: int | None, log_path: Path) -> bool:
    commands = _native_debugger_commands(pid)
    if not commands:
        log_path.write_text("native debugger unavailable on PATH\n", encoding="utf-8")
        return False
    attempts: list[str] = []
    for command in commands:
        try:
            if Path(command[0]).name.lower().startswith("gdb"):
                with tempfile.NamedTemporaryFile(
                    "w", delete=False, suffix=".gdb", encoding="utf-8"
                ) as script_file:
                    script_file.write("set pagination off\n")
                    script_file.write("set confirm off\n")
                    script_file.write("set print demangle on\n")
                    script_file.write("info threads\n")
                    script_file.write("thread apply all bt\n")
                    script_file.write("detach\n")
                    script_file.write("quit\n")
                    script_path = Path(script_file.name)
                try:
                    result = subprocess.run(
                        [*command, "-x", str(script_path)],
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=_PY_SPY_DUMP_TIMEOUT_SECONDS,
                    )
                    rendered_command = [*command, "-x", str(script_path)]
                finally:
                    with suppress(OSError):
                        script_path.unlink()
            else:
                result = subprocess.run(
                    command,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=_PY_SPY_DUMP_TIMEOUT_SECONDS,
                )
                rendered_command = command
        except subprocess.TimeoutExpired:
            attempts.append(
                f"$ {' '.join(command)}\n"
                "native debugger timed out while collecting diagnostics\n"
            )
            continue
        stdout = _demangle_native_debugger_text(result.stdout or "")
        stderr = _demangle_native_debugger_text(result.stderr or "")
        rendered = []
        rendered.append(f"$ {' '.join(rendered_command)}\n")
        rendered.append(f"exit_code={result.returncode}\n")
        if stdout:
            rendered.append("\n[stdout]\n")
            rendered.append(stdout)
            if not stdout.endswith("\n"):
                rendered.append("\n")
        if stderr:
            rendered.append("\n[stderr]\n")
            rendered.append(stderr)
            if not stderr.endswith("\n"):
                rendered.append("\n")
        log_path.write_text("".join(rendered), encoding="utf-8")
        if result.returncode == 0:
            return True
        attempts.append(log_path.read_text(encoding="utf-8"))
    log_path.write_text("\n".join(attempts), encoding="utf-8")
    return False


def _dump_diagnostics(
    *,
    reason: str,
    command: Sequence[str],
    pid: int | None,
    returncode: int | None,
    timeout_seconds: float | None,
    dump_dir: Path,
) -> Path:
    dump_dir.mkdir(parents=True, exist_ok=True)
    stem = _artifact_stem(reason=reason, pid=pid)
    metadata_path = dump_dir / f"{stem}.json"
    py_spy_log_path = dump_dir / f"{stem}.py-spy.log"
    native_debugger_log_path = dump_dir / f"{stem}.native-debugger.log"

    _write_dump_metadata(
        metadata_path=metadata_path,
        reason=reason,
        command=command,
        pid=pid,
        returncode=returncode,
        timeout_seconds=timeout_seconds,
    )
    _run_py_spy_dump(pid=pid, log_path=py_spy_log_path)
    _run_native_debugger_dump(pid=pid, log_path=native_debugger_log_path)
    return metadata_path


def _kill_supervised_process(child: object) -> None:
    pid = getattr(child, "pid", None)
    if pid is not None:
        with suppress(*_SUPERVISOR_CLEANUP_ERRORS):
            kill_process_tree(int(pid))
    kill = getattr(child, "kill", None)
    if callable(kill):
        with suppress(*_SUPERVISOR_CLEANUP_ERRORS):
            kill()
    wait = getattr(child, "wait", None)
    if callable(wait):
        with suppress(*_SUPERVISOR_CLEANUP_ERRORS):
            wait(timeout=5.0)


def _stream_reader(
    source: BinaryIO | None,
    sink: TextIO,
    *,
    touch_activity,
) -> None:
    if source is None:
        return
    try:
        read_chunk = getattr(source, "read1", None)
        if not callable(read_chunk):
            read_chunk = source.read
        while True:
            chunk = read_chunk(4096)
            if not chunk:
                break
            _write_stream_bytes(sink, chunk)
            touch_activity()
    finally:
        close = getattr(source, "close", None)
        if callable(close):
            with suppress(OSError, ValueError):
                close()


def _wait_for_child_with_activity_timeout(
    child: object,
    *,
    timeout: float | None,
) -> tuple[int | None, bool]:
    last_output_at = time.monotonic()
    activity_lock = threading.Lock()

    def touch_activity() -> None:
        nonlocal last_output_at
        with activity_lock:
            last_output_at = time.monotonic()

    stdout_thread = threading.Thread(
        target=_stream_reader,
        args=(getattr(child, "stdout", None), sys.stdout),
        kwargs={"touch_activity": touch_activity},
        name="running-process-stdout-reader",
        daemon=True,
    )
    stderr_thread = threading.Thread(
        target=_stream_reader,
        args=(getattr(child, "stderr", None), sys.stderr),
        kwargs={"touch_activity": touch_activity},
        name="running-process-stderr-reader",
        daemon=True,
    )
    stdout_thread.start()
    stderr_thread.start()

    timed_out = False
    returncode: int | None = None
    try:
        while True:
            poll = getattr(child, "poll", None)
            if callable(poll):
                polled = poll()
                if polled is not None:
                    returncode = int(polled)
                    if not stdout_thread.is_alive() and not stderr_thread.is_alive():
                        break
            if timeout is not None:
                with activity_lock:
                    idle_for = time.monotonic() - last_output_at
                if idle_for >= timeout:
                    timed_out = True
                    break
            if (
                returncode is not None
                and not stdout_thread.is_alive()
                and not stderr_thread.is_alive()
            ):
                break
            time.sleep(0.05)
        if returncode is None and not timed_out:
            wait = getattr(child, "wait", None)
            if callable(wait):
                returncode = int(wait())
            else:
                returncode = 0
    finally:
        stdout_thread.join(timeout=1.0)
        stderr_thread.join(timeout=1.0)
    return returncode, timed_out


def run_command(
    command: Sequence[str],
    *,
    timeout: float | None = None,
    auto_stack_dumping: bool = True,
    stack_dump_dir: Path | None = None,
) -> int:
    child = subprocess.Popen(
        command,
        env=_child_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    dump_dir = _stack_dump_dir(stack_dump_dir)
    returncode, timed_out = _wait_for_child_with_activity_timeout(child, timeout=timeout)
    if timed_out:
        if auto_stack_dumping:
            metadata_path = _dump_diagnostics(
                reason="timeout",
                command=command,
                pid=child.pid,
                returncode=None,
                timeout_seconds=timeout,
                dump_dir=dump_dir,
            )
            _safe_write(
                sys.stderr,
                f"[running-process] timeout diagnostics written to {metadata_path}\n",
            )
        _kill_supervised_process(child)
        return DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE

    if returncode != 0 and auto_stack_dumping:
        metadata_path = _dump_diagnostics(
            reason="abnormal-exit",
            command=command,
            pid=child.pid,
            returncode=returncode,
            timeout_seconds=timeout,
            dump_dir=dump_dir,
        )
        _safe_write(
            sys.stderr,
            f"[running-process] abnormal-exit diagnostics written to {metadata_path}\n",
        )
    return int(returncode)


def main(argv: Sequence[str] | None = None) -> int:
    args = _parse_args(argv)
    command = _normalize_command(args.command)
    return run_command(
        command,
        timeout=args.timeout,
        auto_stack_dumping=not args.no_auto_stack_dumping,
        stack_dump_dir=args.stack_dump_dir,
    )


if __name__ == "__main__":
    sys.exit(main())
