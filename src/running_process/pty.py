from __future__ import annotations

import os
import shlex
import signal
import struct
import subprocess
import sys
import threading
import time
from collections import deque
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any

import psutil

from running_process.exit_status import ExitStatus, ProcessAbnormalExit, classify_exit_status
from running_process.expect import (
    ExpectAction,
    ExpectMatch,
    ExpectPattern,
    apply_expect_action,
    ensure_text,
    search_expect_pattern,
)
from running_process.priority import CpuPriority, normalize_nice

if sys.platform == "win32":
    try:
        import winpty
    except ImportError:
        winpty = None
else:
    import fcntl
    import pty as posix_pty
    import select
    import termios


class PtyNotAvailableError(RuntimeError):
    pass


class InteractiveMode(str, Enum):
    PSEUDO_TERMINAL = "pseudo_terminal"
    CONSOLE_SHARED = "console_shared"
    CONSOLE_ISOLATED = "console_isolated"


@dataclass(frozen=True)
class InteractiveLaunchSpec:
    mode: InteractiveMode
    uses_pty: bool
    ctrl_c_owner: str
    creationflags: int | None
    restore_terminal: bool


@dataclass(frozen=True)
class InterruptResult:
    exit_reason: str
    interrupt_count: int
    returncode: int | None


@dataclass(frozen=True)
class IdleWaitResult:
    reason: str
    idle_for: float
    returncode: int | None


KEYBOARD_INTERRUPT_EXIT_CODES: set[int] = {
    -2,
    -11,
    130,
    255,
    -1073741510,
    3221225786,
    4294967294,
}


class Pty:
    @classmethod
    def is_available(cls) -> bool:
        if sys.platform == "win32":
            return winpty is not None
        return hasattr(os, "read")


class PseudoTerminalProcess:
    def __init__(
        self,
        command: str | list[str],
        *,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: Mapping[str, str] | None = None,
        text: bool = False,
        encoding: str = "utf-8",
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool = True,
        restore_callback: Any | None = None,
        cleanup_callback: Any | None = None,
        auto_run: bool = True,
    ) -> None:
        if not Pty.is_available():
            raise PtyNotAvailableError("Pseudo-terminal support is not available on this platform")
        command, shell = _normalize_command(command, shell)

        self.command = command
        self.shell = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else os.environ.copy()
        self.text = text
        self.encoding = encoding
        self.errors = errors
        self.rows = rows
        self.cols = cols
        self.nice = normalize_nice(nice)
        self.launch_spec = interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
        self.restore_terminal = restore_terminal
        self.restore_callback = restore_callback
        self.cleanup_callback = cleanup_callback

        self._proc: Any | None = None
        self._master_fd: int | None = None
        self._reader_thread: threading.Thread | None = None
        self._chunks: deque[bytes] = deque()
        self._history: list[bytes] = []
        self._closed = False
        self._condition = threading.Condition()
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._restored = False
        self._finalized = False
        self.exit_reason: str | None = None
        self.interrupt_count = 0
        self.interrupted_by_caller = False
        self.last_activity_at: float | None = None
        self._exit_status: ExitStatus | None = None

        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Pseudo-terminal process already started")

        if sys.platform == "win32":
            if winpty is None:
                raise PtyNotAvailableError("winpty is not available on this platform")
            argv = _windows_pty_command(self.command, self.shell)
            self._proc = winpty.PtyProcess.spawn(
                argv,
                cwd=self.cwd,
                env=self.env,
                dimensions=(self.rows, self.cols),
            )
        else:
            argv = _posix_pty_command(self.command, self.shell)
            master_fd, slave_fd = posix_pty.openpty()
            proc = subprocess.Popen(
                argv,
                cwd=self.cwd,
                env=self.env,
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                preexec_fn=os.setsid,
                close_fds=True,
            )
            os.close(slave_fd)
            self._proc = proc
            self._master_fd = master_fd
            self.resize(self.rows, self.cols)

        self._start_time = time.time()
        self.last_activity_at = self._start_time
        _apply_process_nice(self.pid, self.nice)
        self._reader_thread = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader_thread.start()

    def available(self) -> bool:
        with self._condition:
            return bool(self._chunks)

    def read(self, timeout: float | None = None) -> str | bytes:
        deadline = time.time() + timeout if timeout is not None else None
        with self._condition:
            while True:
                if self._chunks:
                    return self._decode(self._chunks.popleft())
                if self._closed:
                    raise EOFError("Pseudo-terminal stream is closed")
                if deadline is not None:
                    remaining = deadline - time.time()
                    if remaining <= 0:
                        raise TimeoutError("No pseudo-terminal output available before timeout")
                    self._condition.wait(remaining)
                else:
                    self._condition.wait()

    def read_non_blocking(self) -> str | bytes | None:
        with self._condition:
            if self._chunks:
                return self._decode(self._chunks.popleft())
            if self._closed:
                raise EOFError("Pseudo-terminal stream is closed")
            return None

    def drain(self) -> list[str | bytes]:
        with self._condition:
            chunks = [self._chunks.popleft() for _ in range(len(self._chunks))]
        return [self._decode(chunk) for chunk in chunks]

    def write(self, data: str | bytes) -> None:
        self._ensure_started()
        raw = data.encode(self.encoding, self.errors) if isinstance(data, str) else data
        if sys.platform == "win32":
            assert self._proc is not None
            text = raw.decode(self.encoding, self.errors).replace("\n", "\r")
            self._proc.write(text)
            return
        assert self._master_fd is not None
        os.write(self._master_fd, raw)

    def resize(self, rows: int, cols: int) -> None:
        self.rows = rows
        self.cols = cols
        if self._proc is None:
            return
        if sys.platform == "win32":
            self._proc.setwinsize(rows, cols)
            return
        if self._master_fd is None:
            return
        size = struct_winsize(rows, cols)
        fcntl.ioctl(self._master_fd, termios.TIOCSWINSZ, size)

    def send_interrupt(self) -> None:
        self._ensure_started()
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        if sys.platform == "win32":
            self._proc.sendintr()
            return
        pid = self.pid
        if pid is not None:
            os.killpg(pid, signal.SIGINT)

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        if sys.platform == "win32":
            return None if self._proc.isalive() else self._proc.exitstatus
        return self._proc.poll()

    def wait(self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False) -> int:
        deadline = time.time() + timeout if timeout is not None else None
        while True:
            code = self.poll()
            if code is not None:
                self._wait_for_reader()
                self._finalize("exit")
                self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
                if code in KEYBOARD_INTERRUPT_EXIT_CODES:
                    raise KeyboardInterrupt
                if raise_on_abnormal_exit and self._exit_status.abnormal:
                    raise ProcessAbnormalExit(self._exit_status)
                return code
            if deadline is not None and time.time() >= deadline:
                self.kill()
                self._finalize("timeout")
                raise TimeoutError("Pseudo-terminal process timed out")
            time.sleep(0.01)

    def terminate(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform == "win32":
            self._proc.terminate()
        else:
            pid = self.pid
            if pid is not None:
                try:
                    os.killpg(pid, signal.SIGTERM)
                except ProcessLookupError:
                    self._finalize("exit")
                    return
        self._wait_for_reader()
        self._finalize("terminate")

    def kill(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform == "win32":
            self._proc.kill(signal.SIGTERM)
        else:
            pid = self.pid
            if pid is not None:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError:
                    self._finalize("exit")
                    return
        self._wait_for_reader()
        self._finalize("kill")

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return int(self._proc.pid)

    @property
    def output(self) -> str | bytes:
        data = b"".join(self._history)
        return self._decode(data)

    @property
    def is_running(self) -> bool:
        return self.poll() is None

    def expect(
        self,
        pattern: ExpectPattern,
        *,
        timeout: float | None = None,
        action: ExpectAction = None,
    ) -> ExpectMatch:
        deadline = time.time() + timeout if timeout is not None else None
        buffer = ensure_text(self.output, self.encoding, self.errors)

        while True:
            match = search_expect_pattern(buffer, pattern)
            if match is not None:
                apply_expect_action(self, action, match)
                return match

            wait_timeout = 0.1
            if deadline is not None:
                remaining = deadline - time.time()
                if remaining <= 0:
                    if self._closed or self.poll() is not None:
                        raise EOFError(
                            f"Pattern not found before stream closed: {pattern!r}"
                        )
                    raise TimeoutError(f"Pattern not found before timeout: {pattern!r}")
                wait_timeout = min(wait_timeout, remaining)

            try:
                chunk = self.read(timeout=wait_timeout)
            except TimeoutError:
                continue
            except EOFError as exc:
                raise EOFError(f"Pattern not found before stream closed: {pattern!r}") from exc
            buffer = f"{buffer}{ensure_text(chunk, self.encoding, self.errors)}"

    @property
    def exit_status(self) -> ExitStatus | None:
        code = self.poll()
        if code is None:
            return None
        if self._exit_status is None:
            self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        return self._exit_status

    def interrupt_and_wait(
        self,
        *,
        grace_timeout: float = 1.0,
        second_interrupt: bool = True,
        terminate_timeout: float | None = None,
        kill_timeout: float | None = None,
    ) -> InterruptResult:
        self.send_interrupt()
        if self._wait_until_exit(grace_timeout):
            return self._interrupt_result("interrupt")
        if second_interrupt:
            self.send_interrupt()
            if self._wait_until_exit(grace_timeout):
                return self._interrupt_result("interrupt")
        if terminate_timeout is not None:
            self.terminate()
            if self._wait_until_exit(terminate_timeout):
                return self._interrupt_result("terminate")
        if kill_timeout is not None:
            self.kill()
            if self._wait_until_exit(kill_timeout):
                return self._interrupt_result("kill")
        return self._interrupt_result("interrupt")

    def wait_for_idle(
        self,
        idle_timeout: float,
        *,
        timeout: float | None = None,
        activity_predicate: Any | None = None,
    ) -> IdleWaitResult:
        predicate = activity_predicate or (lambda _chunk: True)
        start = time.time()
        last_activity = self.last_activity_at or start

        while True:
            if self.poll() is not None:
                return IdleWaitResult("exit", max(0.0, time.time() - last_activity), self.poll())
            if timeout is not None and time.time() - start >= timeout:
                return IdleWaitResult("timeout", max(0.0, time.time() - last_activity), self.poll())
            try:
                chunk = self.read(timeout=min(idle_timeout, 0.1))
            except TimeoutError:
                idle_for = time.time() - last_activity
                if idle_for >= idle_timeout:
                    return IdleWaitResult("idle", idle_for, self.poll())
                continue
            except EOFError:
                return IdleWaitResult("exit", max(0.0, time.time() - last_activity), self.poll())
            if predicate(chunk):
                last_activity = time.time()
                self.last_activity_at = last_activity

    def _reader_loop(self) -> None:
        try:
            while True:
                chunk = self._read_chunk()
                if chunk is None:
                    continue
                if not chunk:
                    break
                with self._condition:
                    self.last_activity_at = time.time()
                    self._history.append(chunk)
                    self._chunks.append(chunk)
                    self._condition.notify_all()
        finally:
            with self._condition:
                self._closed = True
                self._condition.notify_all()
            if self._master_fd is not None:
                try:
                    os.close(self._master_fd)
                except OSError:
                    pass
                self._master_fd = None

    def _read_chunk(self) -> bytes | None:
        if sys.platform == "win32":
            try:
                chunk = self._proc.read(1024)
            except EOFError:
                return b""
            return chunk.encode(self.encoding, self.errors)

        assert self._master_fd is not None
        ready, _, _ = select.select([self._master_fd], [], [], 0.1)
        if not ready:
            if self._proc.poll() is not None:
                return b""
            return None
        try:
            return os.read(self._master_fd, 1024)
        except OSError:
            return b""

    def _ensure_started(self) -> None:
        if self._proc is None:
            raise RuntimeError("Pseudo-terminal process is not running")

    def _wait_for_reader(self) -> None:
        if self._reader_thread is not None:
            self._reader_thread.join(timeout=2)

    def _decode(self, data: bytes) -> str | bytes:
        if not self.text:
            return data
        return data.decode(self.encoding, self.errors)

    def _finalize(self, reason: str) -> None:
        if self._finalized:
            return
        self._finalized = True
        self._end_time = self._end_time or time.time()
        self.exit_reason = (
            "interrupt" if reason == "exit" and self.interrupted_by_caller else reason
        )
        if callable(self.cleanup_callback):
            self.cleanup_callback(self.exit_reason)
        if self.restore_terminal and not self._restored:
            self._restored = True
            if callable(self.restore_callback):
                self.restore_callback()

    def _interrupt_result(self, fallback_reason: str) -> InterruptResult:
        code = self.poll()
        if code is not None:
            self._wait_for_reader()
            self._finalize("exit")
            code = self.poll()
        reason = self.exit_reason or fallback_reason
        self.exit_reason = reason
        return InterruptResult(
            reason,
            self.interrupt_count,
            code,
        )

    def _wait_until_exit(self, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.poll() is not None:
                self._wait_for_reader()
                self._finalize("exit")
                return True
            time.sleep(0.01)
        if self.poll() is not None:
            self._wait_for_reader()
            self._finalize("exit")
            return True
        return False


class InteractiveProcess:
    def __init__(
        self,
        command: str | list[str],
        *,
        mode: InteractiveMode | str = InteractiveMode.CONSOLE_SHARED,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: Mapping[str, str] | None = None,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool | None = None,
        restore_callback: Any | None = None,
        cleanup_callback: Any | None = None,
        auto_run: bool = True,
    ) -> None:
        resolved_mode = InteractiveMode(mode)
        if resolved_mode is InteractiveMode.PSEUDO_TERMINAL:
            raise ValueError("Use PseudoTerminalProcess for pseudo_terminal mode")

        command, shell = _normalize_command(command, shell)
        self.command = command
        self.shell = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else os.environ.copy()
        self.nice = normalize_nice(nice)
        self.launch_spec = interactive_launch_spec(resolved_mode)
        self.restore_terminal = (
            self.launch_spec.restore_terminal
            if restore_terminal is None
            else restore_terminal
        )
        self.restore_callback = restore_callback
        self.cleanup_callback = cleanup_callback
        self._proc: subprocess.Popen[Any] | None = None
        self._end_time: float | None = None
        self._finalized = False
        self.exit_reason: str | None = None
        self.interrupt_count = 0
        self.interrupted_by_caller = False
        self._exit_status: ExitStatus | None = None

        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Interactive process already started")
        popen_command = self.command
        if self.shell:
            if isinstance(self.command, str):
                popen_command = self.command
            else:
                popen_command = subprocess.list2cmdline(self.command)
        popen_kwargs: dict[str, Any] = {
            "cwd": self.cwd,
            "env": self.env,
            "shell": self.shell,
        }
        if sys.platform == "win32":
            popen_kwargs["creationflags"] = (
                self.launch_spec.creationflags or 0
            ) | _windows_priority_class_for_nice(self.nice or 0)
        elif self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            popen_kwargs["start_new_session"] = True
        self._proc = subprocess.Popen(
            popen_command,
            **popen_kwargs,
        )
        if sys.platform != "win32":
            _apply_process_nice(self.pid, self.nice)

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.poll()

    def wait(self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False) -> int:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        try:
            code = self._proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            self.kill()
            self._finalize("timeout")
            raise TimeoutError("Interactive process timed out") from exc
        self._finalize("exit")
        self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def terminate(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform != "win32" and self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            os.killpg(self._proc.pid, signal.SIGTERM)
        else:
            self._proc.terminate()
        self._wait_for_exit()
        self._finalize("terminate")

    def kill(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform != "win32" and self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            os.killpg(self._proc.pid, signal.SIGKILL)
        else:
            self._proc.kill()
        self._wait_for_exit()
        self._finalize("kill")

    def send_interrupt(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        if sys.platform == "win32":
            create_new_process_group = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", None)
            if self.launch_spec.creationflags != create_new_process_group:
                raise RuntimeError(
                    "send_interrupt on Windows requires console_isolated mode"
                )
            os.kill(self._proc.pid, signal.CTRL_BREAK_EVENT)
            return
        if self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            os.killpg(self._proc.pid, signal.SIGINT)
            return
        os.kill(self._proc.pid, signal.SIGINT)

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.pid

    @property
    def exit_status(self) -> ExitStatus | None:
        code = self.poll()
        if code is None:
            return None
        if self._exit_status is None:
            self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        return self._exit_status

    def _finalize(self, reason: str) -> None:
        if self._finalized:
            return
        self._finalized = True
        self._end_time = time.time()
        self.exit_reason = (
            "interrupt" if reason == "exit" and self.interrupted_by_caller else reason
        )
        if callable(self.cleanup_callback):
            self.cleanup_callback(self.exit_reason)
        if self.restore_terminal and callable(self.restore_callback):
            self.restore_callback()

    def _wait_for_exit(self) -> None:
        try:
            self._proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait(timeout=2)


def _windows_pty_command(command: str | list[str], shell: bool) -> list[str]:
    if shell:
        if isinstance(command, str):
            return ["cmd", "/C", command]
        return ["cmd", "/C", subprocess.list2cmdline(command)]
    if isinstance(command, str):
        return [command]
    return command


def _posix_pty_command(command: str | list[str], shell: bool) -> list[str]:
    if shell:
        if isinstance(command, str):
            return ["sh", "-lc", command]
        return ["sh", "-lc", shlex.join(command)]
    if isinstance(command, str):
        return [command]
    return command


def struct_winsize(rows: int, cols: int) -> bytes:
    return struct.pack("HHHH", rows, cols, 0, 0)


def _normalize_command(
    command: str | list[str], shell: bool | None
) -> tuple[str | list[str], bool]:
    if isinstance(command, list):
        return command, bool(shell)

    if shell is True:
        return command, True

    if shell is False:
        return _split_command(command), False

    if _contains_shell_metacharacters(command):
        return command, True
    return _split_command(command), False


def _contains_shell_metacharacters(command: str) -> bool:
    shell_meta = {"&&", "||", "|", ";", ">", "<", "&"}
    return any(token in command for token in shell_meta)


def _split_command(command: str) -> list[str]:
    parts = shlex.split(command, posix=False)
    return [_strip_wrapping_quotes(part) for part in parts]


def _apply_process_nice(pid: int | None, nice: int | None) -> None:
    if pid is None or nice is None:
        return

    try:
        process = psutil.Process(pid)
    except psutil.NoSuchProcess:
        return
    if sys.platform == "win32":
        process.nice(_windows_priority_class_for_nice(nice))
        return
    process.nice(nice)


def _windows_priority_class_for_nice(nice: int) -> int:
    if nice >= 15:
        return psutil.IDLE_PRIORITY_CLASS
    if nice >= 1:
        return psutil.BELOW_NORMAL_PRIORITY_CLASS
    if nice <= -15:
        return psutil.HIGH_PRIORITY_CLASS
    if nice <= -1:
        return psutil.ABOVE_NORMAL_PRIORITY_CLASS
    return psutil.NORMAL_PRIORITY_CLASS


def _strip_wrapping_quotes(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    return value


def interactive_launch_spec(mode: InteractiveMode | str) -> InteractiveLaunchSpec:
    resolved = InteractiveMode(mode)
    create_new_process_group = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", None)
    if resolved is InteractiveMode.PSEUDO_TERMINAL:
        return InteractiveLaunchSpec(
            mode=resolved,
            uses_pty=True,
            ctrl_c_owner="child",
            creationflags=None,
            restore_terminal=True,
        )
    if resolved is InteractiveMode.CONSOLE_ISOLATED:
        return InteractiveLaunchSpec(
            mode=resolved,
            uses_pty=False,
            ctrl_c_owner="parent",
            creationflags=create_new_process_group if sys.platform == "win32" else None,
            restore_terminal=True,
        )
    return InteractiveLaunchSpec(
        mode=resolved,
        uses_pty=False,
        ctrl_c_owner="shared",
        creationflags=None,
        restore_terminal=False,
    )
