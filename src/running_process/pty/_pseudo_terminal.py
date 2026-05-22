from __future__ import annotations

import os
import signal
import sys
import threading
import time
import weakref
from collections.abc import Callable, Mapping
from contextlib import suppress
from dataclasses import replace
from io import TextIOBase
from pathlib import Path
from typing import Any

from running_process._native import (
    NativeIdleDetector,
    NativeProcess,
    NativeProcessMetrics,
    NativePtyBuffer,
    NativeTerminalInput,
)
from running_process.console_encoding import detect_console_encoding, sanitize_for_encoding
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
from running_process.pty._command import _normalize_command, _pty_command, interactive_launch_spec
from running_process.pty._console_io import _safe_console_write_chunk
from running_process.pty._errors import PtyNotAvailableError, SignalBool
from running_process.pty._idle_helpers import (
    _build_default_idle_reset,
    _compile_idle_detector,
    _flush_wait_input,
    _input_contains_newline,
    _invoke_condition_callback,
    _invoke_wait_callback,
    _normalize_wait_conditions,
    _resolve_expect_offset,
    _start_event_count,
)
from running_process.pty._idle_state import (
    _ExpectRuntimeState,
    _IdleRuntimeState,
    _IdleSample,
    _WaitCallbackState,
)
from running_process.pty._process_helpers import (
    _close_native_pty_process,
    _warn_pty_text_mode_ignored,
)
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleContext,
    IdleDecision,
    IdleDetection,
    IdleDetector,
    IdleInfoDiff,
    IdleReachedCallback,
    IdleResetPredicate,
    IdleStartTrigger,
    IdleTiming,
    IdleWaitResult,
    InteractiveMode,
    InterruptResult,
    ProcessIdleDetection,
    PtyIdleDetection,
    WaitCallbackResult,
    WaitCheckpoint,
    WaitCondition,
    WaitForResult,
)

_SUPPORTED_PTY_PLATFORMS = {"win32", "linux", "darwin"}
_PTY_READ_CHUNK_TIMEOUT_SECONDS = 0.01
_PTY_POLL_INTERVAL_SECONDS = 0.001
_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS = 2.0
_PTY_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)

KEYBOARD_INTERRUPT_EXIT_CODES: set[int] = {
    -2,             # Unix: killed by SIGINT (negative signal number)
    130,            # Unix: 128 + SIGINT(2) — shell convention
    -1073741510,    # Windows: STATUS_CONTROL_C_EXIT (signed)
    3221225786,     # Windows: STATUS_CONTROL_C_EXIT (unsigned)
}


class Pty:
    @classmethod
    def is_available(cls) -> bool:
        return sys.platform in _SUPPORTED_PTY_PLATFORMS


class PseudoTerminalProcess:
    def __init__(
        self,
        command: str | list[str],
        *,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: Mapping[str, str] | None = None,
        text: bool = False,
        encoding: str | None = None,
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        capture: bool = True,
        restore_terminal: bool = True,
        expect: list[Expect] | None = None,
        idle_detector: IdleDetector | None = None,
        relay_terminal_input: bool = False,
        arm_idle_timeout_on_submit: bool = False,
        allows_child_ctrl_c_interruption: bool = True,
        auto_run: bool = True,
    ) -> None:
        if not Pty.is_available():
            raise PtyNotAvailableError(
                f"Pseudo-terminal support is not available on unsupported platform: {sys.platform}"
            )
        command, shell = _normalize_command(command, shell)

        if text:
            _warn_pty_text_mode_ignored(env)
        self.command = command
        self.shell = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else os.environ.copy()
        self.text = False
        self.encoding = detect_console_encoding(encoding)
        self.errors = errors
        self.rows = rows
        self.cols = cols
        self.nice = normalize_nice(nice)
        self.capture = bool(capture)
        self.launch_spec = interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
        self.restore_terminal = restore_terminal

        self._proc: NativeProcess | None = None
        self._buffer = (
            NativePtyBuffer(text=False, encoding=self.encoding, errors=self.errors)
            if self.capture
            else None
        )
        self._native_stream_closed = False
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._restored = False
        self._finalized = False
        self.exit_reason: str | None = None
        self.interrupt_count = 0
        self.interrupted_by_caller = False
        self.last_activity_at: float | None = None
        self._exit_status: ExitStatus | None = None
        self._pty_input_bytes_total = 0
        self._pty_newline_events_total = 0
        self._pty_output_bytes_total = 0
        self._pty_control_churn_bytes_total = 0
        self._pty_submit_events_total = 0
        self._pending_echo_chunks: list[bytes] = []
        self._native_idle_detector: NativeIdleDetector | None = None
        self._native_process_metrics: NativeProcessMetrics | None = None
        self._native_exit_watcher: threading.Thread | None = None
        self._close_finalizer: weakref.finalize | None = None
        self._idle_timeout_signal = SignalBool(True)
        self._registered_expect_conditions = list(expect) if expect is not None else []
        self._registered_idle_detector = idle_detector
        self._relay_terminal_input = bool(relay_terminal_input)
        self._arm_idle_timeout_on_submit = bool(arm_idle_timeout_on_submit)
        self._allows_child_ctrl_c_interruption = bool(allows_child_ctrl_c_interruption)
        self._terminal_input_capture: NativeTerminalInput | None = None
        self._terminal_input_thread: threading.Thread | None = None
        self._terminal_input_stop = threading.Event()
        self._terminal_input_restore_state: Any | None = None
        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Pseudo-terminal process already started")

        argv = _pty_command(self.command, self.shell, self.nice)
        self._proc = NativeProcess.for_pty(
            argv,
            cwd=self.cwd,
            env=self.env,
            rows=self.rows,
            cols=self.cols,
            nice=self.nice,
        )
        self._proc.start()

        self._start_time = time.time()
        self.last_activity_at = self._start_time
        if self.pid is not None:
            self._native_process_metrics = NativeProcessMetrics(self.pid)
        self._prime_process_metrics()
        self._close_finalizer = weakref.finalize(self, _close_native_pty_process, self._proc)
        self._native_stream_closed = False
        if self._relay_terminal_input:
            self.start_terminal_input_relay(
                arm_idle_timeout_on_submit=self._arm_idle_timeout_on_submit
            )

    def available(self) -> bool:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return False
        self._pump_native_output(timeout=0.0, consume_all=True)
        return self._buffer.available()

    @property
    def idle_timeout_enabled(self) -> bool:
        return self._idle_timeout_signal.value

    @idle_timeout_enabled.setter
    def idle_timeout_enabled(self, enabled: bool) -> None:
        enabled = bool(enabled)
        detector = self._native_idle_detector
        if detector is not None:
            detector.enabled = enabled
        self._idle_timeout_signal.value = enabled

    def read(self, timeout: float | None = None) -> str | bytes:
        if not self.capture:
            raise NotImplementedError("PTY read() requires capture=True")
        chunk = self.read_non_blocking()
        if chunk is not None:
            return chunk
        _, stream_closed = self._pump_native_output(timeout=timeout, consume_all=False)
        chunk = self.read_non_blocking()
        if chunk is not None:
            return chunk
        if stream_closed or self._native_stream_closed:
            raise EOFError("Pseudo-terminal stream is closed")
        raise TimeoutError("No pseudo-terminal output available before timeout")

    def read_non_blocking(self) -> str | bytes | None:
        if not self.capture:
            raise NotImplementedError("PTY read_non_blocking() requires capture=True")
        self._pump_native_output(timeout=0.0, consume_all=True)
        try:
            return self._buffer.read_non_blocking()
        except RuntimeError as exc:
            if "stream is closed" in str(exc):
                raise EOFError("Pseudo-terminal stream is closed") from exc
            raise

    def read_text(self, timeout: float | None = None) -> str:
        """Like ``read()`` but always returns ``str``, decoded and sanitized for the parent console.

        Use this when the result will be printed to ``sys.stdout``: the value is
        round-tripped through the auto-detected console encoding with
        ``errors='replace'``, so writing it to a cp1252 console will not raise
        ``UnicodeEncodeError`` even when the child emitted UTF-8.
        """
        chunk = self.read(timeout=timeout)
        if isinstance(chunk, bytes):
            chunk = chunk.decode(self.encoding, self.errors)
        return sanitize_for_encoding(chunk, self.encoding)

    def drain(self) -> list[str | bytes]:
        if not self.capture:
            raise NotImplementedError("PTY drain() requires capture=True")
        self._pump_native_output(timeout=0.0, consume_all=True)
        return self._buffer.drain()

    def drain_echo(self) -> list[bytes]:
        self._pump_native_output(timeout=0.0, consume_all=True)
        chunks = list(self._pending_echo_chunks)
        self._pending_echo_chunks.clear()
        return chunks

    def discard_output(self) -> int:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return 0
        self._pump_native_output(timeout=0.0, consume_all=True)
        return int(self._buffer.clear_history())

    @property
    def output_bytes(self) -> int:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return 0
        self._pump_native_output(timeout=0.0, consume_all=True)
        return int(self._buffer.history_bytes())

    def _output_since(self, start: int) -> str | bytes:
        if not self.capture:
            raise NotImplementedError("PTY output capture is disabled")
        self._pump_native_output(timeout=0.0, consume_all=True)
        return self._buffer.output_since(max(0, start))

    def _snapshot_output_history(self) -> tuple[str, int]:
        if not self.capture:
            raise NotImplementedError("PTY output capture is disabled")
        self._pump_native_output(timeout=0.0, consume_all=True)
        return (
            ensure_text(self._buffer.output(), self.encoding, self.errors),
            int(self._buffer.history_bytes()),
        )

    def _snapshot_output_since(self, start: int) -> tuple[str, int]:
        if not self.capture:
            raise NotImplementedError("PTY output capture is disabled")
        self._pump_native_output(timeout=0.0, consume_all=True)
        return (
            ensure_text(
                self._buffer.output_since(max(0, start)),
                self.encoding,
                self.errors,
            ),
            int(self._buffer.history_bytes()),
        )

    def write(self, data: str | bytes, *, submit: bool = False) -> None:
        self._ensure_started()
        raw = data.encode(self.encoding, self.errors) if isinstance(data, str) else data
        self._pty_input_bytes_total += len(raw)
        if _input_contains_newline(raw):
            self._pty_newline_events_total += 1
        if submit:
            self._pty_submit_events_total += 1
        self.last_activity_at = time.time()
        if self._native_idle_detector is not None:
            self._native_idle_detector.record_input(len(raw))
        assert self._proc is not None
        self._proc.write(raw, submit=submit)
        self._sync_native_input_metrics()

    def submit(self, data: str | bytes = "\n") -> None:
        self.write(data, submit=True)

    @property
    def terminal_input_relay_active(self) -> bool:
        if (
            sys.platform == "win32"
            and self._proc is not None
            and hasattr(self._proc, "terminal_input_relay_active")
        ):
            active = bool(self._proc.terminal_input_relay_active())
            self._sync_native_input_metrics()
            return active
        thread = self._terminal_input_thread
        return thread is not None and thread.is_alive()

    def _sync_native_input_metrics(self) -> None:
        if self._proc is None or not hasattr(self._proc, "pty_input_bytes_total"):
            return
        # Only sync when we need to detect submit events for idle timeout arming.
        if not self._arm_idle_timeout_on_submit or self.idle_timeout_enabled:
            return
        input_bytes_total = int(self._proc.pty_input_bytes_total())
        newline_events_total = int(self._proc.pty_newline_events_total())
        submit_events_total = int(self._proc.pty_submit_events_total())
        submit_delta = submit_events_total - self._pty_submit_events_total
        self._pty_input_bytes_total = input_bytes_total
        self._pty_newline_events_total = newline_events_total
        self._pty_submit_events_total = submit_events_total
        if submit_delta > 0:
            self.idle_timeout_enabled = True

    def _maybe_arm_idle_timeout_from_terminal_input(self, *, submit: bool) -> None:
        if not self._arm_idle_timeout_on_submit and not submit:
            return
        if not submit or self.idle_timeout_enabled:
            return
        self.idle_timeout_enabled = True

    def _start_windows_terminal_input_relay(self) -> None:
        if (
            self._allows_child_ctrl_c_interruption
            and self._proc is not None
            and hasattr(self._proc, "start_terminal_input_relay")
        ):
            self._proc.start_terminal_input_relay()
            self._sync_native_input_metrics()
            return
        capture = NativeTerminalInput()
        capture.start()
        self._terminal_input_capture = capture
        filter_ctrl_c = not self._allows_child_ctrl_c_interruption

        def relay() -> None:
            try:
                while not self._terminal_input_stop.is_set() and self.poll() is None:
                    try:
                        data, submit = capture.read_batch(timeout=0.05)
                    except TimeoutError:
                        continue
                    if filter_ctrl_c:
                        data = data.replace(b"\x03", b"")
                        if not data:
                            continue
                    self._maybe_arm_idle_timeout_from_terminal_input(submit=submit)
                    self.write(data, submit=submit)
            finally:
                with suppress(Exception):
                    capture.close()

        self._terminal_input_thread = threading.Thread(
            target=relay,
            daemon=True,
            name=f"pty-terminal-input-{self.pid or 'pending'}",
        )
        self._terminal_input_thread.start()

    def _start_posix_terminal_input_relay(self) -> None:
        import select
        import termios
        import tty

        if not sys.stdin.isatty():
            return

        stdin_fd = sys.stdin.fileno()
        previous_state = termios.tcgetattr(stdin_fd)
        tty.setraw(stdin_fd)
        self._terminal_input_restore_state = (stdin_fd, previous_state)
        filter_ctrl_c = not self._allows_child_ctrl_c_interruption

        def relay() -> None:
            try:
                while not self._terminal_input_stop.is_set() and self.poll() is None:
                    try:
                        ready, _, _ = select.select([stdin_fd], [], [], 0.05)
                    except (OSError, ValueError):
                        return
                    if not ready:
                        continue
                    data = os.read(stdin_fd, 65536)
                    if not data:
                        continue
                    # Drain any additional data already in the fd buffer
                    # so large pastes arrive as a single write.
                    while True:
                        try:
                            more_ready, _, _ = select.select([stdin_fd], [], [], 0)
                        except (OSError, ValueError):
                            break
                        if not more_ready:
                            break
                        more = os.read(stdin_fd, 65536)
                        if not more:
                            break
                        data += more
                    if filter_ctrl_c:
                        data = data.replace(b"\x03", b"")
                        if not data:
                            continue
                    submit = b"\r" in data or b"\n" in data
                    self._maybe_arm_idle_timeout_from_terminal_input(submit=submit)
                    self.write(data, submit=submit)
            finally:
                self._restore_posix_terminal_input()

        self._terminal_input_thread = threading.Thread(
            target=relay,
            daemon=True,
            name=f"pty-terminal-input-{self.pid or 'pending'}",
        )
        self._terminal_input_thread.start()

    def _restore_posix_terminal_input(self) -> None:
        state = self._terminal_input_restore_state
        if state is None:
            return
        self._terminal_input_restore_state = None
        import termios

        stdin_fd, previous_state = state
        with suppress(Exception):
            termios.tcsetattr(stdin_fd, termios.TCSANOW, previous_state)

    def start_terminal_input_relay(
        self,
        *,
        arm_idle_timeout_on_submit: bool | None = None,
    ) -> None:
        self._ensure_started()
        if self.terminal_input_relay_active:
            return
        if arm_idle_timeout_on_submit is not None:
            self._arm_idle_timeout_on_submit = bool(arm_idle_timeout_on_submit)
        self._terminal_input_stop = threading.Event()
        if sys.platform == "win32":
            self._start_windows_terminal_input_relay()
            return
        self._start_posix_terminal_input_relay()

    def stop_terminal_input_relay(self) -> None:
        self._terminal_input_stop.set()
        if (
            sys.platform == "win32"
            and self._proc is not None
            and hasattr(self._proc, "stop_terminal_input_relay")
        ):
            with suppress(Exception):
                self._proc.stop_terminal_input_relay()
            self._sync_native_input_metrics()
        thread = self._terminal_input_thread
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout=0.2)
        self._terminal_input_thread = None
        capture = self._terminal_input_capture
        self._terminal_input_capture = None
        if capture is not None:
            with suppress(Exception):
                capture.close()
        self._restore_posix_terminal_input()

    def resize(self, rows: int, cols: int) -> None:
        self.rows = rows
        self.cols = cols
        if self._proc is None:
            return
        self._proc.resize(rows, cols)

    def send_interrupt(self) -> None:
        self._ensure_started()
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        assert self._proc is not None
        self._proc.send_interrupt()

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.poll()

    def wait(self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False) -> int:
        self._ensure_started()
        assert self._proc is not None
        try:
            code = self._wait_for_exit_code(timeout=timeout)
        except TimeoutError:
            self.kill()
            self._finalize("timeout")
            raise TimeoutError("Pseudo-terminal process timed out") from None

        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("exit")
        self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def terminate(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        assert self._proc is not None
        self._proc.terminate()
        with suppress(TimeoutError, RuntimeError):
            self._wait_for_exit_code(timeout=2.0)
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("terminate")

    def kill(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform != "win32" and self.pid is not None:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except (OSError, AttributeError):
                pass
            else:
                with suppress(TimeoutError, RuntimeError):
                    self._wait_for_exit_code(timeout=2.0)
                with suppress(*_PTY_CLEANUP_ERRORS):
                    self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
                self._finalize("kill")
                return
        assert self._proc is not None
        self._proc.kill()
        with suppress(TimeoutError, RuntimeError):
            self._wait_for_exit_code(timeout=2.0)
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("kill")

    def close(self) -> None:
        if self._proc is None:
            return
        if self._finalized:
            return
        with suppress(*_PTY_CLEANUP_ERRORS):
            if self.poll() is None:
                self._proc.close()
                self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
                self._finalize("close")
                return
            self._drain_native_until_eof(timeout=0.1)
            self._finalize("exit")

    def __del__(self) -> None:
        with suppress(*_PTY_CLEANUP_ERRORS):
            self.close()

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.pid

    @property
    def output(self) -> str | bytes:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return b""
        self._pump_native_output(timeout=0.0, consume_all=True)
        value = self._buffer.output()
        if isinstance(value, str):
            return sanitize_for_encoding(value, self.encoding)
        return value

    @property
    def output_text(self) -> str:
        """Captured output decoded to ``str`` and sanitized for the parent console.

        Always safe to ``print()`` even when the parent console is cp1252 and
        the child emitted UTF-8.
        """
        raw = self.output
        if isinstance(raw, bytes):
            raw = raw.decode(self.encoding, self.errors)
        return sanitize_for_encoding(raw, self.encoding)

    def checkpoint(self) -> WaitCheckpoint:
        if not self.capture:
            raise NotImplementedError("PTY checkpoint() requires capture=True")
        return WaitCheckpoint(len(ensure_text(self.output, self.encoding, self.errors)))

    def wait_for_expect(
        self,
        next_expect: Expect | None = None,
        *,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> WaitForResult:
        if not self.capture:
            raise NotImplementedError("PTY wait_for_expect() requires capture=True")
        active_expect_conditions = list(self._registered_expect_conditions)
        if not active_expect_conditions:
            if next_expect is None:
                raise ValueError("No registered Expect conditions are configured for this process")
            active_expect_conditions = [next_expect]
            next_expect = None
        result = self.wait_for(
            *active_expect_conditions,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo_output,
        )
        if not result.matched:
            self._registered_expect_conditions = active_expect_conditions
            return result
        if next_expect is None:
            self._registered_expect_conditions = []
            return result
        offset = self.checkpoint().offset
        if result.expect_match is not None:
            offset = result.expect_match.span[1]
        self._registered_expect_conditions = [
            replace(next_expect, after=WaitCheckpoint(offset))
        ]
        return result

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
        if not self.capture:
            raise NotImplementedError("PTY expect() requires capture=True")
        deadline = time.time() + timeout if timeout is not None else None
        buffer, history_bytes = self._snapshot_output_history()

        while True:
            match = search_expect_pattern(buffer, pattern)
            if match is not None:
                apply_expect_action(self, action, match)
                return match

            wait_timeout = 0.1
            if deadline is not None:
                remaining = deadline - time.time()
                if remaining <= 0:
                    if self.poll() is not None:
                        raise EOFError(
                            f"Pattern not found before stream closed: {pattern!r}"
                        )
                    raise TimeoutError(f"Pattern not found before timeout: {pattern!r}")
                wait_timeout = min(wait_timeout, remaining)

            try:
                chunk = self.read(timeout=wait_timeout)
            except TimeoutError:
                new_output, current_history_bytes = self._snapshot_output_since(history_bytes)
                if current_history_bytes > history_bytes:
                    buffer = f"{buffer}{new_output}"
                    history_bytes = current_history_bytes
                    continue
                code = self.poll()
                if code is not None:
                    self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
                    self._finalize("exit")
                    self._exit_status = classify_exit_status(
                        code, KEYBOARD_INTERRUPT_EXIT_CODES
                    )
                    new_output, current_history_bytes = self._snapshot_output_since(history_bytes)
                    if current_history_bytes > history_bytes:
                        buffer = f"{buffer}{new_output}"
                        history_bytes = current_history_bytes
                        continue
                    raise EOFError(
                        f"Pattern not found before stream closed: {pattern!r}"
                    ) from None
                continue
            except EOFError as exc:
                raise EOFError(f"Pattern not found before stream closed: {pattern!r}") from exc
            buffer = f"{buffer}{ensure_text(chunk, self.encoding, self.errors)}"
            history_bytes = int(self._buffer.history_bytes())

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
            second_interrupt_timeout = max(grace_timeout, 1.0)
            if self._wait_until_exit(second_interrupt_timeout):
                return self._interrupt_result("interrupt")
            if terminate_timeout is None and kill_timeout is None:
                self.kill()
                return self._interrupt_result("kill")
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
        idle_detector: IdleDetector | None = None,
        *,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> IdleWaitResult:
        if idle_detector is None:
            idle_detector = self._registered_idle_detector
        timing, idle_reached, predicate = _compile_idle_detector(idle_detector)
        if timing is None or (idle_reached is None and predicate is None):
            code = self.wait(timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit)
            return IdleWaitResult(
                returncode=code,
                idle_detected=False,
                exit_reason="process_exit",
                idle_for_seconds=0.0,
            )

        # The native idle-detector fast path is not safe after the Phase 3
        # reader-thread removal. Native PTY chunks are now staged in Rust and
        # must be pumped through `_handle_native_chunk` from Python to update
        # idle accounting. Until idle orchestration moves fully native, keep
        # the observable behavior correct by using the Python wait loop here.

        default_predicate = _build_default_idle_reset(idle_detector) if isinstance(
            idle_detector, IdleDetection
        ) else _build_default_idle_reset(IdleDetection())

        start = time.time()
        deadline = start + timeout if timeout is not None else None
        state = _IdleRuntimeState(last_reset_at=start, stable_since=None)
        idle_timeout_enabled = self.idle_timeout_enabled
        idle_process_cfg = (
            idle_detector.process if isinstance(idle_detector, IdleDetection) else None
        )
        start_trigger = (
            idle_detector.pty.start_trigger
            if isinstance(idle_detector, IdleDetection) and idle_detector.pty is not None
            else IdleStartTrigger.IMMEDIATE
        )
        start_events_seen = _start_event_count(self, start_trigger)
        idle_armed = (
            start_trigger is IdleStartTrigger.IMMEDIATE or start_events_seen > 0
        )
        previous = self._sample_idle_snapshot(process_cfg=idle_process_cfg)

        try:
            while True:
                if echo_output:
                    self._echo_to_console(sys.stdout)

                now = time.time()
                if self.idle_timeout_enabled != idle_timeout_enabled:
                    idle_timeout_enabled = self.idle_timeout_enabled
                    if idle_timeout_enabled:
                        state.last_reset_at = now
                        state.stable_since = None
                if deadline is not None and now >= deadline:
                    return IdleWaitResult(
                        returncode=self.poll(),
                        idle_detected=False,
                        exit_reason="timeout",
                        idle_for_seconds=max(0.0, now - state.last_reset_at),
                    )

                wait_timeout = timing.sample_interval_seconds
                if deadline is not None:
                    wait_timeout = min(wait_timeout, max(0.0, deadline - now))
                if wait_timeout > 0:
                    self._pump_native_output(timeout=wait_timeout, consume_all=True)

                current = self._sample_idle_snapshot(process_cfg=idle_process_cfg)
                diff = IdleInfoDiff(
                    delta_seconds=max(0.0, current.sampled_at - previous.sampled_at),
                    process_alive=current.process_alive,
                    pty_input_bytes=current.pty_input_bytes - previous.pty_input_bytes,
                    pty_output_bytes=current.pty_output_bytes - previous.pty_output_bytes,
                    pty_control_churn_bytes=(
                        current.pty_control_churn_bytes - previous.pty_control_churn_bytes
                    ),
                    cpu_percent=current.cpu_percent,
                    disk_io_bytes=current.disk_io_bytes - previous.disk_io_bytes,
                    network_io_bytes=current.network_io_bytes - previous.network_io_bytes,
                )
                previous = current

                sample_now = current.sampled_at
                if not idle_armed and start_trigger is not IdleStartTrigger.IMMEDIATE:
                    current_start_events = _start_event_count(self, start_trigger)
                    if current_start_events != start_events_seen:
                        start_events_seen = current_start_events
                        idle_armed = True
                        state.last_reset_at = sample_now
                        state.stable_since = None
                        self.last_activity_at = sample_now
                    else:
                        code = current.returncode
                        if code is not None:
                            self._drain_native_until_eof(
                                timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                            )
                            self._finalize("exit")
                            self._exit_status = classify_exit_status(
                                code, KEYBOARD_INTERRUPT_EXIT_CODES
                            )
                            interrupted = code in KEYBOARD_INTERRUPT_EXIT_CODES
                            if (
                                raise_on_abnormal_exit
                                and self._exit_status.abnormal
                                and not interrupted
                            ):
                                raise ProcessAbnormalExit(self._exit_status)
                            return IdleWaitResult(
                                returncode=code,
                                idle_detected=False,
                                exit_reason="interrupt" if interrupted else "process_exit",
                                idle_for_seconds=0.0,
                            )
                        continue

                stable_for = 0.0
                if state.stable_since is not None:
                    stable_for = max(0.0, sample_now - state.stable_since)
                ctx = IdleContext(
                    idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                    stable_for_seconds=stable_for,
                    sample_count=state.sample_count,
                )
                state.sample_count += 1

                handled = False
                if idle_reached is not None:
                    decision = idle_reached(diff)
                    if not isinstance(decision, IdleDecision):
                        raise TypeError("idle_reached callback must return an IdleDecision")
                    if decision is IdleDecision.DEFAULT:
                        handled = False
                    elif decision is IdleDecision.IS_IDLE:
                        return IdleWaitResult(
                            returncode=self.poll(),
                            idle_detected=True,
                            exit_reason="idle_timeout",
                            idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                        )
                    elif decision is IdleDecision.ACTIVE:
                        state.last_reset_at = sample_now
                        state.stable_since = None
                        self.last_activity_at = sample_now
                        handled = True
                    elif decision is IdleDecision.BEGIN_IDLE and state.stable_since is None:
                        idle_started_at = max(0.0, sample_now - diff.delta_seconds)
                        state.last_reset_at = idle_started_at
                        state.stable_since = idle_started_at
                        handled = True
                    elif decision is IdleDecision.BEGIN_IDLE:
                        handled = True
                    if handled and (
                        idle_timeout_enabled
                        and state.stable_since is not None
                        and max(0.0, sample_now - state.last_reset_at) >= timing.timeout_seconds
                    ):
                        return IdleWaitResult(
                            returncode=self.poll(),
                            idle_detected=True,
                            exit_reason="idle_timeout",
                            idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                        )
                if not handled:
                    if (
                        (predicate is not None and predicate(diff, ctx))
                        or (idle_reached is not None and default_predicate(diff, ctx))
                    ):
                        state.last_reset_at = sample_now
                        state.stable_since = None
                        self.last_activity_at = sample_now
                    else:
                        if state.stable_since is None:
                            state.stable_since = sample_now
                        idle_for = max(0.0, sample_now - state.last_reset_at)
                        stable_for = max(0.0, sample_now - state.stable_since)
                        if (
                            idle_timeout_enabled
                            and idle_for >= timing.timeout_seconds
                            and stable_for >= timing.stability_window_seconds
                        ):
                            return IdleWaitResult(
                                returncode=self.poll(),
                                idle_detected=True,
                                exit_reason="idle_timeout",
                                idle_for_seconds=idle_for,
                            )

                code = current.returncode
                if code is not None:
                    self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
                    self._finalize("exit")
                    self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
                    interrupted = code in KEYBOARD_INTERRUPT_EXIT_CODES
                    if raise_on_abnormal_exit and self._exit_status.abnormal and not interrupted:
                        raise ProcessAbnormalExit(self._exit_status)
                    return IdleWaitResult(
                        returncode=code,
                        idle_detected=False,
                        exit_reason="interrupt" if interrupted else "process_exit",
                        idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                    )
        finally:
            pass

    def wait_for(
        self,
        *conditions: (
            WaitCondition
            | Callable[..., object]
            | list[WaitCondition | Callable[..., object]]
            | tuple[WaitCondition | Callable[..., object], ...]
        ),
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> WaitForResult:
        wait_conditions = _normalize_wait_conditions(*conditions)
        loop_iterations = 0
        sleep_ns = 0
        expect_scan_ns = 0
        expect_scan_count = 0
        history_update_ns = 0
        history_update_count = 0

        if not wait_conditions:
            code = self.wait(timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit)
            return WaitForResult(returncode=code, matched=False, exit_reason="process_exit")

        idle_conditions = [
            condition for condition in wait_conditions if isinstance(condition, Idle)
        ]
        if len(idle_conditions) > 1:
            raise ValueError("wait_for supports at most one Idle condition")

        if (
            len(wait_conditions) == 1
            and isinstance(wait_conditions[0], Idle)
            and wait_conditions[0].on_callback is None
        ):
            idle_condition = wait_conditions[0]
            idle_result = self.wait_for_idle(
                idle_condition.detector,
                timeout=timeout,
                raise_on_abnormal_exit=raise_on_abnormal_exit,
                echo_output=echo_output,
            )
            return WaitForResult(
                returncode=idle_result.returncode,
                matched=idle_result.idle_detected,
                exit_reason=(
                    "condition_met"
                    if idle_result.idle_detected
                    else (
                        "interrupt"
                        if idle_result.exit_reason == "interrupt"
                        else idle_result.exit_reason
                    )
                ),
                condition=idle_condition if idle_result.idle_detected else None,
                idle_result=idle_result,
            )

        idle_condition = idle_conditions[0] if idle_conditions else None
        expect_conditions = [
            condition for condition in wait_conditions if isinstance(condition, Expect)
        ]
        if expect_conditions and not self.capture:
            raise NotImplementedError("PTY wait_for() Expect conditions require capture=True")
        expect_states: list[tuple[Expect, _ExpectRuntimeState]] = [
            (
                condition,
                _ExpectRuntimeState(search_offset=_resolve_expect_offset(condition, self)),
            )
            for condition in expect_conditions
        ]
        callback_conditions = [
            condition for condition in wait_conditions if isinstance(condition, Callback)
        ]

        timing: IdleTiming | None = None
        idle_reached: IdleReachedCallback | None = None
        predicate: IdleResetPredicate | None = None
        default_predicate: IdleResetPredicate | None = None
        idle_state: _IdleRuntimeState | None = None
        idle_timeout_enabled = self.idle_timeout_enabled
        previous: _IdleSample | None = None
        process_cfg: ProcessIdleDetection | None = None
        start_trigger = IdleStartTrigger.IMMEDIATE
        start_events_seen = _start_event_count(self, start_trigger)
        idle_armed = idle_condition is not None
        next_idle_sample_at: float | None = None

        if idle_condition is not None:
            timing, idle_reached, predicate = _compile_idle_detector(idle_condition.detector)
            if timing is None or (idle_reached is None and predicate is None):
                raise ValueError("Idle condition requires an active idle detector")
            if isinstance(idle_condition.detector, IdleDetection):
                default_predicate = _build_default_idle_reset(idle_condition.detector)
                process_cfg = idle_condition.detector.process
                if idle_condition.detector.pty is not None:
                    start_trigger = idle_condition.detector.pty.start_trigger
            else:
                default_predicate = _build_default_idle_reset(IdleDetection())
            started = time.time()
            idle_state = _IdleRuntimeState(last_reset_at=started, stable_since=None)
            previous = self._sample_idle_snapshot(process_cfg=process_cfg)
            next_idle_sample_at = started + timing.sample_interval_seconds
            start_events_seen = _start_event_count(self, start_trigger)
            idle_armed = (
                start_trigger is IdleStartTrigger.IMMEDIATE or start_events_seen > 0
            )

        callback_states: list[tuple[Callback, _WaitCallbackState]] = []
        callback_threads: list[threading.Thread] = []
        stop_callbacks = threading.Event()

        for condition in callback_conditions:
            state = _WaitCallbackState()
            callback_states.append((condition, state))

            def run_callback(
                callback_condition: Callback = condition,
                callback_state: _WaitCallbackState = state,
            ) -> None:
                while not stop_callbacks.is_set():
                    try:
                        result, pending_writes = _invoke_wait_callback(
                            callback_condition.callback, self
                        )
                    except BaseException as exc:
                        callback_state.error = exc
                        if isinstance(exc, KeyboardInterrupt):
                            import _thread
                            _thread.interrupt_main()
                        return
                    if pending_writes:
                        with callback_state.lock:
                            callback_state.pending_writes.extend(pending_writes)
                    if result:
                        callback_state.result = result
                        callback_state.ready.store(True)
                        return
                    if stop_callbacks.wait(max(0.001, callback_condition.poll_interval_seconds)):
                        return

            thread = threading.Thread(target=run_callback, daemon=True)
            thread.start()
            callback_threads.append(thread)

        deadline = time.time() + timeout if timeout is not None else None
        if self.capture:
            buffer, history_bytes = self._snapshot_output_history()
        else:
            buffer, history_bytes = "", 0

        try:
            while True:
                loop_iterations += 1
                if echo_output:
                    self._echo_to_console(sys.stdout)

                if self.capture:
                    new_output, current_history_bytes = self._snapshot_output_since(history_bytes)
                    if current_history_bytes > history_bytes:
                        history_update_start = time.perf_counter_ns()
                        buffer = f"{buffer}{new_output}"
                        history_bytes = current_history_bytes
                        history_update_count += 1
                        history_update_ns += time.perf_counter_ns() - history_update_start
                for condition, state in expect_states:
                    if not state.armed:
                        continue
                    scoped_buffer = buffer[state.search_offset :]
                    scan_start = time.perf_counter_ns()
                    suppress_match = (
                        search_expect_pattern(scoped_buffer, condition.NOT)
                        if condition.NOT is not None
                        else None
                    )
                    match = search_expect_pattern(scoped_buffer, condition.pattern)
                    expect_scan_count += 1
                    expect_scan_ns += time.perf_counter_ns() - scan_start
                    if suppress_match is not None and (
                        match is None or suppress_match.span[0] <= match.span[0]
                    ):
                        state.search_offset += suppress_match.span[1]
                        state.armed = False
                        continue
                    if match is None:
                        continue
                    adjusted_match = ExpectMatch(
                        buffer=buffer,
                        matched=match.matched,
                        span=(
                            state.search_offset + match.span[0],
                            state.search_offset + match.span[1],
                        ),
                        groups=match.groups,
                    )
                    state.search_offset = adjusted_match.span[1]
                    apply_expect_action(self, condition.action, adjusted_match)
                    if condition.on_callback is not None:
                        action, pending_writes = _invoke_condition_callback(
                            condition.on_callback, adjusted_match, self
                        )
                        _flush_wait_input(self, pending_writes)
                        if action is WaitCallbackResult.CONTINUE:
                            continue
                        if action is WaitCallbackResult.CONTINUE_AND_DISARM:
                            state.armed = False
                            continue
                    return WaitForResult(
                        returncode=self.poll(),
                        matched=True,
                        exit_reason="condition_met",
                        condition=condition,
                        expect_match=adjusted_match,
                    )

                for condition, state in callback_states:
                    if state.error is not None:
                        raise state.error
                    with state.lock:
                        pending_writes = list(state.pending_writes)
                        state.pending_writes.clear()
                    if pending_writes:
                        _flush_wait_input(self, pending_writes)
                    if state.ready.load():
                        return WaitForResult(
                            returncode=self.poll(),
                            matched=True,
                            exit_reason="condition_met",
                            condition=condition,
                            callback_result=state.result,
                        )

                code = self.poll()
                if code is not None:
                    self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
                    self._finalize("exit")
                    self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
                    if self.capture:
                        new_output, current_history_bytes = (
                            self._snapshot_output_since(history_bytes)
                        )
                        if current_history_bytes > history_bytes:
                            history_update_start = time.perf_counter_ns()
                            buffer = f"{buffer}{new_output}"
                            history_bytes = current_history_bytes
                            history_update_count += 1
                            history_update_ns += time.perf_counter_ns() - history_update_start
                            continue
                    if code in KEYBOARD_INTERRUPT_EXIT_CODES:
                        raise KeyboardInterrupt
                    if raise_on_abnormal_exit and self._exit_status.abnormal:
                        raise ProcessAbnormalExit(self._exit_status)
                    return WaitForResult(
                        returncode=code,
                        matched=False,
                        exit_reason="process_exit",
                    )

                now = time.time()
                if deadline is not None and now >= deadline:
                    return WaitForResult(
                        returncode=self.poll(),
                        matched=False,
                        exit_reason="timeout",
                    )

                if (
                    idle_armed
                    and idle_state is not None
                    and self.idle_timeout_enabled != idle_timeout_enabled
                ):
                    idle_timeout_enabled = self.idle_timeout_enabled
                    if idle_timeout_enabled:
                        idle_state.last_reset_at = now
                        idle_state.stable_since = None

                if (
                    idle_armed
                    and timing is not None
                    and idle_state is not None
                    and previous is not None
                    and next_idle_sample_at is not None
                    and now >= next_idle_sample_at
                ):
                    current = self._sample_idle_snapshot(process_cfg=process_cfg)
                    diff = IdleInfoDiff(
                        delta_seconds=max(0.0, current.sampled_at - previous.sampled_at),
                        process_alive=current.process_alive,
                        pty_input_bytes=current.pty_input_bytes - previous.pty_input_bytes,
                        pty_output_bytes=current.pty_output_bytes - previous.pty_output_bytes,
                        pty_control_churn_bytes=(
                            current.pty_control_churn_bytes - previous.pty_control_churn_bytes
                        ),
                        cpu_percent=current.cpu_percent,
                        disk_io_bytes=current.disk_io_bytes - previous.disk_io_bytes,
                        network_io_bytes=current.network_io_bytes - previous.network_io_bytes,
                    )
                    previous = current
                    sample_now = current.sampled_at
                    next_idle_sample_at = sample_now + timing.sample_interval_seconds

                    if not idle_armed and start_trigger is not IdleStartTrigger.IMMEDIATE:
                        current_start_events = _start_event_count(self, start_trigger)
                        if current_start_events != start_events_seen:
                            start_events_seen = current_start_events
                            idle_armed = True
                            idle_state.last_reset_at = sample_now
                            idle_state.stable_since = None
                            self.last_activity_at = sample_now
                        else:
                            continue

                    stable_for = 0.0
                    if idle_state.stable_since is not None:
                        stable_for = max(0.0, sample_now - idle_state.stable_since)
                    ctx = IdleContext(
                        idle_for_seconds=max(0.0, sample_now - idle_state.last_reset_at),
                        stable_for_seconds=stable_for,
                        sample_count=idle_state.sample_count,
                    )
                    idle_state.sample_count += 1

                    handled = False
                    idle_detected = False
                    if idle_reached is not None:
                        decision = idle_reached(diff)
                        if not isinstance(decision, IdleDecision):
                            raise TypeError("idle_reached callback must return an IdleDecision")
                        if decision is IdleDecision.ACTIVE:
                            idle_state.last_reset_at = sample_now
                            idle_state.stable_since = None
                            self.last_activity_at = sample_now
                            handled = True
                        elif decision is IdleDecision.BEGIN_IDLE:
                            if idle_state.stable_since is None:
                                idle_started_at = max(0.0, sample_now - diff.delta_seconds)
                                idle_state.last_reset_at = idle_started_at
                                idle_state.stable_since = idle_started_at
                            handled = True
                        elif decision is IdleDecision.IS_IDLE:
                            idle_detected = True

                    if not handled and not idle_detected:
                        should_reset = False
                        if predicate is not None and predicate(diff, ctx):
                            should_reset = True
                        elif idle_reached is not None and default_predicate is not None:
                            should_reset = default_predicate(diff, ctx)

                        if should_reset:
                            idle_state.last_reset_at = sample_now
                            idle_state.stable_since = None
                            self.last_activity_at = sample_now
                        else:
                            if idle_state.stable_since is None:
                                idle_state.stable_since = sample_now
                            idle_for = max(0.0, sample_now - idle_state.last_reset_at)
                            stable_for = max(0.0, sample_now - idle_state.stable_since)
                            if (
                                idle_timeout_enabled
                                and idle_for >= timing.timeout_seconds
                                and stable_for >= timing.stability_window_seconds
                            ):
                                idle_detected = True

                    if idle_detected:
                        idle_result = IdleWaitResult(
                            returncode=self.poll(),
                            idle_detected=True,
                            exit_reason="idle_timeout",
                            idle_for_seconds=max(0.0, sample_now - idle_state.last_reset_at),
                        )
                        if idle_condition is not None and idle_condition.on_callback is not None:
                            action, pending_writes = _invoke_condition_callback(
                                idle_condition.on_callback, idle_result, self
                            )
                            _flush_wait_input(self, pending_writes)
                            if action is WaitCallbackResult.CONTINUE:
                                idle_state.last_reset_at = sample_now
                                idle_state.stable_since = None
                                self.last_activity_at = sample_now
                                continue
                            if action is WaitCallbackResult.CONTINUE_AND_DISARM:
                                idle_armed = False
                                continue
                        return WaitForResult(
                            returncode=idle_result.returncode,
                            matched=True,
                            exit_reason="condition_met",
                            condition=idle_condition,
                            idle_result=idle_result,
                        )

                sleep_for = _PTY_POLL_INTERVAL_SECONDS
                if callback_conditions:
                    sleep_for = min(
                        sleep_for,
                        min(
                            max(0.001, condition.poll_interval_seconds)
                            for condition in callback_conditions
                        ),
                    )
                if deadline is not None:
                    sleep_for = min(sleep_for, max(0.0, deadline - time.time()))
                if sleep_for > 0:
                    sleep_start = time.perf_counter_ns()
                    self._pump_native_output(timeout=sleep_for, consume_all=True)
                    sleep_ns += time.perf_counter_ns() - sleep_start
        finally:
            stop_callbacks.set()
            for thread in callback_threads:
                thread.join(timeout=0.2)

    def _read_chunk(self, *, timeout: float | None = None) -> bytes | None:
        try:
            assert self._proc is not None
            wait_timeout = _PTY_READ_CHUNK_TIMEOUT_SECONDS if timeout is None else timeout
            return self._proc.read_chunk(timeout=wait_timeout)
        except TimeoutError:
            return None
        except RuntimeError as exc:
            if "stream is closed" in str(exc):
                return b""
            raise

    def _ensure_started(self) -> None:
        if self._proc is None:
            raise RuntimeError("Pseudo-terminal process is not running")

    def _mark_native_stream_closed(self) -> None:
        if self._native_stream_closed:
            return
        self._native_stream_closed = True
        if self._buffer is not None:
            self._buffer.close()

    def _handle_native_chunk(self, chunk: bytes) -> None:
        if self._proc is not None:
            with suppress(RuntimeError):
                self._proc.respond_to_queries(chunk)
        # Output accounting (visible bytes, control churn) is now tracked
        # by the Rust reader thread via atomic counters.  Python only needs
        # to update the activity timestamp and echo/buffer bookkeeping.
        self.last_activity_at = time.time()
        self._pending_echo_chunks.append(chunk)
        if self._buffer is not None:
            self._buffer.record_output(chunk)
        if self._native_idle_detector is not None:
            self._native_idle_detector.record_output(chunk)

    def _echo_to_console(self, stream: TextIOBase) -> None:
        for chunk in self.drain_echo():
            _safe_console_write_chunk(
                stream,
                chunk,
                encoding=self.encoding,
                errors=self.errors,
            )

    def _pump_native_output(
        self,
        *,
        timeout: float | None,
        consume_all: bool,
    ) -> tuple[bool, bool]:
        if self._proc is None or self._native_stream_closed:
            return False, self._native_stream_closed
        read_any = False
        wait_timeout = timeout
        while True:
            chunk = self._read_chunk(timeout=wait_timeout)
            if chunk is None:
                return read_any, False
            if not chunk:
                self._mark_native_stream_closed()
                return read_any, True
            self._handle_native_chunk(chunk)
            read_any = True
            if not consume_all:
                return read_any, False
            wait_timeout = 0.0

    def _drain_native_until_eof(self, *, timeout: float) -> None:
        if self._proc is None or self._native_stream_closed:
            return
        deadline = time.monotonic() + max(0.0, timeout)
        first_wait = True
        while not self._native_stream_closed:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            wait_timeout = remaining if first_wait else min(0.05, remaining)
            first_wait = False
            self._pump_native_output(timeout=wait_timeout, consume_all=True)
        watcher_thread = self._native_exit_watcher
        if watcher_thread is not None:
            watcher_thread.join(timeout=2)

    def _prime_process_metrics(self) -> None:
        metrics = self._native_process_metrics
        if metrics is None:
            return
        metrics.prime()

    def _sample_idle_snapshot(self, process_cfg: ProcessIdleDetection | None) -> _IdleSample:
        self._sync_native_input_metrics()
        now = time.time()
        cpu_percent = 0.0
        disk_io_bytes = 0
        network_io_bytes = 0
        if process_cfg is not None and self._native_process_metrics is not None:
            process_alive, cpu_percent, disk_io_bytes, network_io_bytes = (
                self._native_process_metrics.sample()
            )
        else:
            process_alive = self.poll() is None

        # Read output accounting from Rust reader thread (atomic counters).
        output_bytes = self._pty_output_bytes_total
        churn_bytes = self._pty_control_churn_bytes_total
        if self._proc is not None:
            with suppress(AttributeError):
                output_bytes = int(self._proc.pty_output_bytes_total())
            with suppress(AttributeError):
                churn_bytes = int(self._proc.pty_control_churn_bytes_total())

        return _IdleSample(
            sampled_at=now,
            process_alive=process_alive,
            pty_input_bytes=self._pty_input_bytes_total,
            pty_output_bytes=output_bytes,
            pty_control_churn_bytes=churn_bytes,
            cpu_percent=cpu_percent,
            disk_io_bytes=disk_io_bytes,
            network_io_bytes=network_io_bytes,
            returncode=self.poll(),
        )

    def _wait_for_idle_native(
        self,
        idle_detector: IdleDetection,
        *,
        timeout: float | None,
    ) -> IdleWaitResult:
        pty_cfg = idle_detector.pty or PtyIdleDetection()
        initial_idle_for = 0.0
        if self.last_activity_at is not None:
            initial_idle_for = max(0.0, time.time() - self.last_activity_at)
        self._native_idle_detector = NativeIdleDetector(
            idle_detector.timing.timeout_seconds,
            idle_detector.timing.stability_window_seconds,
            idle_detector.timing.sample_interval_seconds,
            self._idle_timeout_signal._native,
            pty_cfg.reset_on_input,
            pty_cfg.reset_on_output,
            pty_cfg.count_control_churn_as_output,
            initial_idle_for,
        )
        self._start_native_exit_watcher()
        idle_detected, reason, idle_for_seconds, returncode = self._native_idle_detector.wait(
            timeout=timeout
        )
        self._native_idle_detector = None
        if returncode is not None:
            self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
            self._finalize("exit")
            self._exit_status = classify_exit_status(returncode, KEYBOARD_INTERRUPT_EXIT_CODES)
        return IdleWaitResult(
            returncode=returncode,
            idle_detected=idle_detected,
            exit_reason=reason,  # type: ignore[arg-type]
            idle_for_seconds=idle_for_seconds,
        )

    def _start_native_exit_watcher(self) -> None:
        detector = self._native_idle_detector
        if detector is None:
            return
        process_ref = weakref.ref(self)

        def watch_for_exit() -> None:
            while True:
                process = process_ref()
                if process is None:
                    return
                code = process.poll()
                if code is not None:
                    detector.mark_exit(code, code in KEYBOARD_INTERRUPT_EXIT_CODES)
                    return
                time.sleep(0.05)  # 50ms — exit detection doesn't need 1ms precision

        self._native_exit_watcher = threading.Thread(
            target=watch_for_exit,
            daemon=True,
            name=f"pty-exit-watcher-{self.pid or 'pending'}",
        )
        self._native_exit_watcher.start()

    def _decode(self, data: bytes) -> str | bytes:
        if not self.text:
            return data
        return data.decode(self.encoding, self.errors)

    def _finalize(self, reason: str) -> None:
        if self._finalized:
            return
        self.stop_terminal_input_relay()
        self._finalized = True
        self._end_time = self._end_time or time.time()
        self.exit_reason = (
            "interrupt" if reason == "exit" and self.interrupted_by_caller else reason
        )
        if self.restore_terminal and not self._restored:
            self._restored = True

    def _interrupt_result(self, fallback_reason: str) -> InterruptResult:
        code = self.poll()
        if code is not None:
            self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
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
        self._ensure_started()
        try:
            self._wait_for_exit_code(timeout=timeout)
        except TimeoutError:
            return False
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("exit")
        return True

    def _wait_for_exit_code(self, *, timeout: float | None) -> int:
        self._ensure_started()
        deadline = None if timeout is None else time.monotonic() + max(0.0, timeout)
        while True:
            code = self.poll()
            if code is not None:
                return code
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Pseudo-terminal process timed out")
                wait_timeout = min(0.05, remaining)
            else:
                wait_timeout = 0.05
            self._pump_native_output(timeout=wait_timeout, consume_all=True)
