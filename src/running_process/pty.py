from __future__ import annotations

import inspect
import os
import re
import shlex
import sys
import threading
import time
import weakref
from collections.abc import Callable, Mapping
from contextlib import suppress
from dataclasses import dataclass, field, replace
from enum import Enum
from io import TextIOBase
from pathlib import Path
from typing import Any, Literal

from running_process._native import (
    NativeIdleDetector,
    NativeProcessMetrics,
    NativePtyBuffer,
    NativePtyProcess,
    NativeRunningProcess,
    NativeSignalBool,
    native_apply_process_nice,
)
from running_process.command_render import list2cmdline as render_command_list
from running_process.compat import CREATE_NEW_PROCESS_GROUP
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

_SUPPORTED_PTY_PLATFORMS = {"win32", "linux", "darwin"}


class PtyNotAvailableError(RuntimeError):
    pass


class SignalBool:
    def __init__(self, value: bool = False) -> None:
        self._value = bool(value)
        self._native = NativeSignalBool(self._value)

    @property
    def value(self) -> bool:
        return self._value

    @value.setter
    def value(self, value: bool) -> None:
        self._value = bool(value)
        self._native.value = self._value

    def load(self) -> bool:
        return self._native.load_nolock()

    def store(self, value: bool) -> None:
        self.value = value

    def compare_and_swap(self, current: bool, new: bool) -> bool:
        swapped = self._native.compare_and_swap_locked(bool(current), bool(new))
        if swapped:
            self._value = bool(new)
        else:
            self._value = self._native.load_nolock()
        return swapped

    def __bool__(self) -> bool:
        return self._value


def _safe_console_write(stream: TextIOBase, line: str | bytes) -> None:
    text = line.decode("utf-8", errors="replace") if isinstance(line, bytes) else line
    try:
        stream.write(text)
        stream.write("\n")
    except UnicodeEncodeError:
        encoding = stream.encoding or "utf-8"
        rendered = text.encode(encoding, errors="replace")
        if hasattr(stream, "buffer"):
            stream.buffer.write(rendered + b"\n")
        else:
            stream.write(rendered.decode(encoding, errors="replace"))
            stream.write("\n")
    stream.flush()


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


class IdleDecision(str, Enum):
    DEFAULT = "default"
    ACTIVE = "active"
    BEGIN_IDLE = "begin_idle"
    IS_IDLE = "is_idle"


@dataclass(slots=True)
class IdleTiming:
    timeout_seconds: float = 10.0
    stability_window_seconds: float = 0.75
    sample_interval_seconds: float = 0.25


@dataclass(slots=True)
class PtyIdleDetection:
    reset_on_input: bool = True
    reset_on_output: bool = True
    count_control_churn_as_output: bool = True


@dataclass(slots=True)
class ProcessIdleDetection:
    cpu_percent_before_reset: float = 2.0
    max_disk_io_bytes_before_reset: int = 4096
    max_network_bytes_before_reset: int = 4096


@dataclass(slots=True)
class IdleInfoDiff:
    delta_seconds: float
    process_alive: bool
    pty_input_bytes: int = 0
    pty_output_bytes: int = 0
    pty_control_churn_bytes: int = 0
    cpu_percent: float = 0.0
    disk_io_bytes: int = 0
    network_io_bytes: int = 0

    @property
    def interval_seconds(self) -> float:
        return self.delta_seconds


@dataclass(slots=True)
class IdleContext:
    idle_for_seconds: float
    stable_for_seconds: float
    sample_count: int


IdleDiff = IdleInfoDiff
IdleReachedCallback = Callable[[IdleInfoDiff], IdleDecision]
IdleResetPredicate = Callable[[IdleInfoDiff, IdleContext], bool]


@dataclass(slots=True)
class IdleDetection:
    timing: IdleTiming = field(default_factory=IdleTiming)
    pty: PtyIdleDetection | None = field(default_factory=PtyIdleDetection)
    process: ProcessIdleDetection | None = None
    idle_reached: IdleReachedCallback | None = None
    predicate: IdleResetPredicate | None = None


IdleDetector = IdleDetection | IdleResetPredicate | None


@dataclass(frozen=True, slots=True)
class IdleWaitResult:
    returncode: int | None
    idle_detected: bool
    exit_reason: Literal["process_exit", "idle_timeout", "timeout", "interrupt"]
    idle_for_seconds: float = 0.0

    @property
    def reason(self) -> str:
        return self.exit_reason

    @property
    def idle_for(self) -> float:
        return self.idle_for_seconds


@dataclass(frozen=True, slots=True)
class Idle:
    detector: IdleDetector = field(default_factory=IdleDetection)
    on_callback: Callable[..., object] | None = None


class WaitCallbackResult(str, Enum):
    EXIT = "exit"
    CONTINUE = "continue"
    CONTINUE_AND_DISARM = "continue_and_disarm"


@dataclass(frozen=True, slots=True)
class Expect:
    pattern: ExpectPattern
    action: ExpectAction = None
    NOT: ExpectPattern | None = None
    after: WaitCheckpoint | Literal["start", "now"] = "start"
    on_callback: Callable[..., object] | None = None


@dataclass(frozen=True, slots=True)
class Callback:
    callback: Callable[..., object]
    poll_interval_seconds: float = 0.05


WaitCondition = Idle | Expect | Callback


@dataclass(frozen=True, slots=True)
class WaitForResult:
    returncode: int | None
    matched: bool
    exit_reason: Literal["condition_met", "process_exit", "timeout", "interrupt"]
    condition: WaitCondition | None = None
    expect_match: ExpectMatch | None = None
    idle_result: IdleWaitResult | None = None
    callback_result: object | None = None

    @property
    def reason(self) -> str:
        return self.exit_reason


@dataclass(frozen=True, slots=True)
class WaitCheckpoint:
    offset: int


class WaitInputBuffer:
    def __init__(self) -> None:
        self._items: list[str | bytes] = []

    def write(self, data: str | bytes) -> None:
        self._items.append(data)

    def drain(self) -> list[str | bytes]:
        items = list(self._items)
        self._items.clear()
        return items

    def __bool__(self) -> bool:
        return bool(self._items)


@dataclass(slots=True)
class _IdleRuntimeState:
    last_reset_at: float
    stable_since: float | None
    sample_count: int = 0


@dataclass(slots=True)
class _IdleSample:
    sampled_at: float
    process_alive: bool
    pty_input_bytes: int
    pty_output_bytes: int
    pty_control_churn_bytes: int
    cpu_percent: float
    disk_io_bytes: int
    network_io_bytes: int
    returncode: int | None


@dataclass(slots=True)
class _TerminalControlStripper:
    _pending: bytearray = field(default_factory=bytearray)
    _string_terminator: bytes | None = None

    def strip(self, chunk: bytes) -> bytes:
        if not chunk and not self._pending:
            return b""
        data = bytes(self._pending) + bytes(chunk)
        self._pending.clear()
        output = bytearray()
        index = 0

        while index < len(data):
            if self._string_terminator is not None:
                terminator = self._string_terminator
                terminator_index = data.find(terminator, index)
                if terminator_index == -1:
                    self._pending.extend(data[index:])
                    break
                index = terminator_index + len(terminator)
                self._string_terminator = None
                continue

            byte = data[index]
            if byte == 0x1B:
                if index + 1 >= len(data):
                    self._pending.append(byte)
                    break
                marker = data[index + 1]
                if marker == ord("["):
                    end = _find_csi_end(data, index + 2)
                    if end is None:
                        self._pending.extend(data[index:])
                        break
                    index = end + 1
                    continue
                if marker == ord("]"):
                    self._string_terminator = b"\x07"
                    index += 2
                    continue
                if marker in {ord("P"), ord("X"), ord("^"), ord("_")}:
                    self._string_terminator = b"\x1b\\"
                    index += 2
                    continue
                index += 2
                continue
            if byte in {0x08, 0x7F}:
                index += 1
                continue
            output.append(byte)
            index += 1

        return bytes(output)


def _find_csi_end(data: bytes, start: int) -> int | None:
    for index in range(start, len(data)):
        current = data[index]
        if 0x40 <= current <= 0x7E:
            return index
    return None


_TERMINAL_FRAGMENT_RE = re.compile(rb"(?:\d+;){2,}\d+_")


def _strip_terminal_fragments(chunk: bytes) -> bytes:
    if not chunk:
        return chunk
    return _TERMINAL_FRAGMENT_RE.sub(b"", chunk)


@dataclass(slots=True)
class _IdleCallbackThreadState:
    pending_diff: IdleInfoDiff | None = None
    inflight: bool = False
    latest_decision: IdleDecision | None = None
    error: BaseException | None = None
    closed: bool = False


@dataclass(slots=True)
class _WaitCallbackState:
    ready: SignalBool = field(default_factory=SignalBool)
    result: object | None = None
    error: BaseException | None = None
    pending_writes: list[str | bytes] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)


@dataclass(slots=True)
class _ExpectRuntimeState:
    search_offset: int = 0
    armed: bool = True


KEYBOARD_INTERRUPT_EXIT_CODES: set[int] = {
    -2,
    -11,
    130,
    255,
    -1073741510,
    3221225786,
    4294967294,
}


def _compile_idle_detector(
    idle_detector: IdleDetector,
) -> tuple[IdleTiming | None, IdleReachedCallback | None, IdleResetPredicate | None]:
    if idle_detector is None:
        return None, None, None
    if isinstance(idle_detector, IdleDetection):
        if idle_detector.idle_reached is not None and idle_detector.predicate is not None:
            raise ValueError("idle_reached and predicate are mutually exclusive")
        if idle_detector.idle_reached is not None:
            return idle_detector.timing, idle_detector.idle_reached, None
        predicate = idle_detector.predicate or _build_default_idle_reset(idle_detector)
        return idle_detector.timing, None, predicate
    if callable(idle_detector):
        callback_arity = _callable_arity(idle_detector)
        if callback_arity == 1:
            return IdleTiming(), idle_detector, None
        if callback_arity == 2:
            return IdleTiming(), None, idle_detector
        raise TypeError("idle_detector callable must accept 1 or 2 positional arguments")
    raise TypeError(
        "idle_detector must be None, an IdleDetection instance, or a callable callback"
    )


def _callable_arity(callback: Callable[..., Any]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs:
        if len(required_positional) <= 1:
            return 1
        if len(required_positional) == 2:
            return 2
    if len(required_positional) in {1, 2}:
        return len(required_positional)
    raise TypeError("idle_detector callable must accept 1 or 2 positional arguments")


def _wait_callback_arity(callback: Callable[..., object]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs and len(required_positional) <= 2:
        return len(required_positional)
    if len(required_positional) in {0, 1, 2}:
        return len(required_positional)
    raise TypeError("wait callback must accept 0, 1, or 2 positional arguments")


def _invoke_wait_callback(
    callback: Callable[..., object], process: PseudoTerminalProcess
) -> tuple[object, list[str | bytes]]:
    arity = _wait_callback_arity(callback)
    input_buffer = WaitInputBuffer()
    if arity == 0:
        result = callback()
    elif arity == 1:
        result = callback(input_buffer)
    else:
        result = callback(input_buffer, process)
    return result, input_buffer.drain()


def _condition_callback_arity(callback: Callable[..., object]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs and len(required_positional) <= 3:
        return len(required_positional)
    if len(required_positional) in {0, 1, 2, 3}:
        return len(required_positional)
    raise TypeError("condition on_callback must accept 0, 1, 2, or 3 positional arguments")


def _invoke_condition_callback(
    callback: Callable[..., object],
    payload: object,
    process: PseudoTerminalProcess,
) -> tuple[WaitCallbackResult, list[str | bytes]]:
    arity = _condition_callback_arity(callback)
    input_buffer = WaitInputBuffer()
    if arity == 0:
        result = callback()
    elif arity == 1:
        result = callback(payload)
    elif arity == 2:
        result = callback(payload, input_buffer)
    else:
        result = callback(payload, input_buffer, process)
    if not isinstance(result, WaitCallbackResult):
        raise TypeError("condition on_callback must return a WaitCallbackResult")
    return result, input_buffer.drain()


def _normalize_wait_conditions(
    *conditions: (
        WaitCondition
        | Callable[..., object]
        | list[WaitCondition | Callable[..., object]]
        | tuple[WaitCondition | Callable[..., object], ...]
    ),
) -> list[WaitCondition]:
    normalized: list[WaitCondition] = []
    for condition in conditions:
        if isinstance(condition, (Idle, Expect, Callback)):
            normalized.append(condition)
            continue
        if callable(condition):
            normalized.append(Callback(condition))
            continue
        if isinstance(condition, (list, tuple)):
            for nested in condition:
                if isinstance(nested, (Idle, Expect, Callback)):
                    normalized.append(nested)
                    continue
                if callable(nested):
                    normalized.append(Callback(nested))
                    continue
                raise TypeError("wait_for conditions must be Idle, Expect, Callback, or a callable")
            continue
        raise TypeError("wait_for conditions must be Idle, Expect, Callback, or a callable")
    return normalized


def _flush_wait_input(
    process: PseudoTerminalProcess, items: list[str | bytes]
) -> None:
    for item in items:
        process.write(item)


def _resolve_expect_offset(
    condition: Expect, process: PseudoTerminalProcess
) -> int:
    if condition.after == "start":
        return 0
    if condition.after == "now":
        return len(ensure_text(process.output, process.encoding, process.errors))
    return max(0, condition.after.offset)


def _build_default_idle_reset(cfg: IdleDetection) -> IdleResetPredicate:
    return lambda diff, ctx: _default_idle_reset(diff, ctx, cfg)


def _default_idle_reset(diff: IdleDiff, _ctx: IdleContext, cfg: IdleDetection) -> bool:
    pty_cfg = cfg.pty
    if pty_cfg is not None:
        if pty_cfg.reset_on_input and diff.pty_input_bytes > 0:
            return True
        output_bytes = diff.pty_output_bytes
        if pty_cfg.count_control_churn_as_output:
            output_bytes += diff.pty_control_churn_bytes
        if pty_cfg.reset_on_output and output_bytes > 0:
            return True

    process_cfg = cfg.process
    if process_cfg is not None:
        if diff.cpu_percent > process_cfg.cpu_percent_before_reset:
            return True
        if diff.disk_io_bytes > process_cfg.max_disk_io_bytes_before_reset:
            return True
        if diff.network_io_bytes > process_cfg.max_network_bytes_before_reset:
            return True

    return False


def _merge_idle_diff(base: IdleInfoDiff, update: IdleInfoDiff) -> IdleInfoDiff:
    total_delta = base.delta_seconds + update.delta_seconds
    weighted_cpu = 0.0
    if total_delta > 0:
        weighted_cpu = (
            (base.cpu_percent * base.delta_seconds) + (update.cpu_percent * update.delta_seconds)
        ) / total_delta
    return IdleInfoDiff(
        delta_seconds=total_delta,
        process_alive=update.process_alive,
        pty_input_bytes=base.pty_input_bytes + update.pty_input_bytes,
        pty_output_bytes=base.pty_output_bytes + update.pty_output_bytes,
        pty_control_churn_bytes=base.pty_control_churn_bytes + update.pty_control_churn_bytes,
        cpu_percent=weighted_cpu,
        disk_io_bytes=base.disk_io_bytes + update.disk_io_bytes,
        network_io_bytes=base.network_io_bytes + update.network_io_bytes,
    )


def _control_churn_bytes(chunk: bytes) -> int:
    total = 0
    index = 0
    while index < len(chunk):
        byte = chunk[index]
        if byte == 0x1B:
            start = index
            index += 1
            if index < len(chunk) and chunk[index] == ord("["):
                index += 1
                while index < len(chunk):
                    current = chunk[index]
                    index += 1
                    if 0x40 <= current <= 0x7E:
                        break
            total += index - start
            continue
        if byte in {0x08, 0x0D, 0x7F}:
            total += 1
        index += 1
    return total


def _close_native_pty_process(proc: NativePtyProcess | None) -> None:
    if proc is None:
        return
    with suppress(Exception):
        proc.close()


def _run_pty_reader_loop(process_ref: weakref.ReferenceType[PseudoTerminalProcess]) -> None:
    try:
        while True:
            process = process_ref()
            if process is None:
                return
            try:
                chunk = process._read_chunk()
                if chunk is None:
                    continue
                if not chunk:
                    break
                if process._proc is not None:
                    process._proc.respond_to_queries(chunk)
                control_bytes = _control_churn_bytes(chunk)
                rendered_chunk = process._terminal_control_stripper.strip(chunk)
                if control_bytes and rendered_chunk:
                    rendered_chunk = _strip_terminal_fragments(rendered_chunk)
                process.last_activity_at = time.time()
                process._pty_output_bytes_total += max(0, len(chunk) - control_bytes)
                process._pty_control_churn_bytes_total += control_bytes
                if rendered_chunk:
                    process._buffer.record_output(rendered_chunk)
                if process._native_idle_detector is not None:
                    process._native_idle_detector.record_output(chunk)
            finally:
                del process
    finally:
        process = process_ref()
        if process is not None:
            process._buffer.close()


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
        encoding: str = "utf-8",
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool = True,
        expect: list[Expect] | None = None,
        idle_detector: IdleDetector | None = None,
        auto_run: bool = True,
    ) -> None:
        if not Pty.is_available():
            raise PtyNotAvailableError(
                f"Pseudo-terminal support is not available on unsupported platform: {sys.platform}"
            )
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

        self._proc: NativePtyProcess | None = None
        self._reader_thread: threading.Thread | None = None
        self._buffer = NativePtyBuffer(text=self.text, encoding=self.encoding, errors=self.errors)
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
        self._pty_output_bytes_total = 0
        self._pty_control_churn_bytes_total = 0
        self._terminal_control_stripper = _TerminalControlStripper()
        self._native_idle_detector: NativeIdleDetector | None = None
        self._native_process_metrics: NativeProcessMetrics | None = None
        self._native_exit_watcher: threading.Thread | None = None
        self._close_finalizer: weakref.finalize | None = None
        self._idle_timeout_signal = SignalBool(True)
        self._registered_expect_conditions = list(expect) if expect is not None else []
        self._registered_idle_detector = idle_detector

        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Pseudo-terminal process already started")

        argv = _pty_command(self.command, self.shell)
        self._proc = NativePtyProcess(
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
        _apply_process_nice(self.pid, self.nice)
        if self.pid is not None:
            self._native_process_metrics = NativeProcessMetrics(self.pid)
        self._prime_process_metrics()
        self._close_finalizer = weakref.finalize(self, _close_native_pty_process, self._proc)
        self._reader_thread = threading.Thread(
            target=_run_pty_reader_loop,
            args=(weakref.ref(self),),
            daemon=True,
        )
        self._reader_thread.start()

    def available(self) -> bool:
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
        try:
            return self._buffer.read(timeout=timeout)
        except RuntimeError as exc:
            if "stream is closed" in str(exc):
                raise EOFError("Pseudo-terminal stream is closed") from exc
            raise

    def read_non_blocking(self) -> str | bytes | None:
        try:
            return self._buffer.read_non_blocking()
        except RuntimeError as exc:
            if "stream is closed" in str(exc):
                raise EOFError("Pseudo-terminal stream is closed") from exc
            raise

    def drain(self) -> list[str | bytes]:
        return self._buffer.drain()

    def discard_output(self) -> int:
        return int(self._buffer.clear_history())

    @property
    def output_bytes(self) -> int:
        return int(self._buffer.history_bytes())

    def _output_since(self, start: int) -> str | bytes:
        return self._buffer.output_since(max(0, start))

    def write(self, data: str | bytes) -> None:
        self._ensure_started()
        raw = data.encode(self.encoding, self.errors) if isinstance(data, str) else data
        self._pty_input_bytes_total += len(raw)
        self.last_activity_at = time.time()
        if self._native_idle_detector is not None:
            self._native_idle_detector.record_input(len(raw))
        assert self._proc is not None
        self._proc.write(raw)

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
        assert self._proc is not None
        self._proc.terminate()
        self._wait_for_reader()
        self._finalize("terminate")

    def kill(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        assert self._proc is not None
        self._proc.kill()
        with suppress(RuntimeError):
            self._proc.wait(timeout=2.0)
        self._wait_for_reader()
        self._finalize("kill")

    def close(self) -> None:
        if self._proc is None:
            return
        if self._finalized:
            return
        with suppress(Exception):
            if self.poll() is None:
                self._proc.close()
                self._wait_for_reader()
                self._finalize("close")
                return
            self._wait_for_reader()
            self._finalize("exit")

    def __del__(self) -> None:
        with suppress(Exception):
            self.close()

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.pid

    @property
    def output(self) -> str | bytes:
        return self._buffer.output()

    def checkpoint(self) -> WaitCheckpoint:
        return WaitCheckpoint(len(ensure_text(self.output, self.encoding, self.errors)))

    def wait_for_expect(
        self,
        next_expect: Expect | None = None,
        *,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> WaitForResult:
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
        deadline = time.time() + timeout if timeout is not None else None
        buffer = ensure_text(self.output, self.encoding, self.errors)
        history_bytes = self.output_bytes

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
                current_history_bytes = self.output_bytes
                if current_history_bytes > history_bytes:
                    new_output = ensure_text(
                        self._output_since(history_bytes),
                        self.encoding,
                        self.errors,
                    )
                    buffer = (
                        f"{buffer}"
                        f"{new_output}"
                    )
                    history_bytes = current_history_bytes
                continue
            except EOFError as exc:
                raise EOFError(f"Pattern not found before stream closed: {pattern!r}") from exc
            buffer = f"{buffer}{ensure_text(chunk, self.encoding, self.errors)}"
            history_bytes = self.output_bytes

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

        if (
            isinstance(idle_detector, IdleDetection)
            and idle_detector.idle_reached is None
            and idle_detector.predicate is None
            and idle_detector.process is None
        ):
            return self._wait_for_idle_native(idle_detector, timeout=timeout)

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
        previous = self._sample_idle_snapshot(process_cfg=idle_process_cfg)

        try:
            while True:
                if echo_output:
                    for chunk in self.drain():
                        _safe_console_write(sys.stdout, chunk)

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
                    time.sleep(wait_timeout)

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
                    self._wait_for_reader()
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
        idle_armed = idle_condition is not None
        next_idle_sample_at: float | None = None

        if idle_condition is not None:
            timing, idle_reached, predicate = _compile_idle_detector(idle_condition.detector)
            if timing is None or (idle_reached is None and predicate is None):
                raise ValueError("Idle condition requires an active idle detector")
            if isinstance(idle_condition.detector, IdleDetection):
                default_predicate = _build_default_idle_reset(idle_condition.detector)
                process_cfg = idle_condition.detector.process
            else:
                default_predicate = _build_default_idle_reset(IdleDetection())
            started = time.time()
            idle_state = _IdleRuntimeState(last_reset_at=started, stable_since=None)
            previous = self._sample_idle_snapshot(process_cfg=process_cfg)
            next_idle_sample_at = started + timing.sample_interval_seconds

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
        buffer = ensure_text(self.output, self.encoding, self.errors)
        history_bytes = self.output_bytes

        try:
            while True:
                if echo_output:
                    for chunk in self.drain():
                        _safe_console_write(sys.stdout, chunk)

                current_history_bytes = self.output_bytes
                if current_history_bytes > history_bytes:
                    new_output = ensure_text(
                        self._output_since(history_bytes),
                        self.encoding,
                        self.errors,
                    )
                    buffer = (
                        f"{buffer}"
                        f"{new_output}"
                    )
                    history_bytes = current_history_bytes
                for condition, state in expect_states:
                    if not state.armed:
                        continue
                    scoped_buffer = buffer[state.search_offset :]
                    suppress_match = (
                        search_expect_pattern(scoped_buffer, condition.NOT)
                        if condition.NOT is not None
                        else None
                    )
                    match = search_expect_pattern(scoped_buffer, condition.pattern)
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
                    self._wait_for_reader()
                    self._finalize("exit")
                    self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
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

                sleep_for = 0.01
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
                    time.sleep(sleep_for)
        finally:
            stop_callbacks.set()
            for thread in callback_threads:
                thread.join(timeout=0.2)

    def _read_chunk(self) -> bytes | None:
        try:
            assert self._proc is not None
            return self._proc.read_chunk(timeout=0.1)
        except TimeoutError:
            return None
        except RuntimeError as exc:
            if "stream is closed" in str(exc):
                return b""
            raise

    def _ensure_started(self) -> None:
        if self._proc is None:
            raise RuntimeError("Pseudo-terminal process is not running")

    def _wait_for_reader(self) -> None:
        if self._reader_thread is not None:
            self._reader_thread.join(timeout=2)
        if self._native_exit_watcher is not None:
            self._native_exit_watcher.join(timeout=2)

    def _prime_process_metrics(self) -> None:
        metrics = self._native_process_metrics
        if metrics is None:
            return
        metrics.prime()

    def _sample_idle_snapshot(self, process_cfg: ProcessIdleDetection | None) -> _IdleSample:
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

        return _IdleSample(
            sampled_at=now,
            process_alive=process_alive,
            pty_input_bytes=self._pty_input_bytes_total,
            pty_output_bytes=self._pty_output_bytes_total,
            pty_control_churn_bytes=self._pty_control_churn_bytes_total,
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
            self._wait_for_reader()
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
                time.sleep(0.01)

        self._native_exit_watcher = threading.Thread(target=watch_for_exit, daemon=True)
        self._native_exit_watcher.start()

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
        if self.restore_terminal and not self._restored:
            self._restored = True

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
        self._proc: NativeRunningProcess | None = None
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
        creationflags = self.launch_spec.creationflags if sys.platform == "win32" else None
        self._proc = NativeRunningProcess(
            self.command,
            cwd=self.cwd,
            env=self.env,
            shell=self.shell,
            capture=False,
            creationflags=creationflags,
            nice=self.nice,
            create_process_group=(
                sys.platform != "win32"
                and self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED
            ),
        )
        self._proc.start()

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.poll()

    def wait(self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False) -> int:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        try:
            code = self._proc.wait(timeout=timeout)
        except TimeoutError as exc:
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
        if self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            self._proc.terminate_group()
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
        if self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            self._proc.kill_group()
        else:
            self._proc.kill()
        self._wait_for_exit()
        self._finalize("kill")

    def close(self) -> None:
        if self._proc is None or self._finalized:
            return
        with suppress(Exception):
            if self.poll() is None:
                self.kill()
                return
            self._finalize("exit")

    def __del__(self) -> None:
        with suppress(Exception):
            self.close()

    def send_interrupt(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        self._proc.send_interrupt()

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

    def _wait_for_exit(self) -> None:
        try:
            self._proc.wait(timeout=2)
        except TimeoutError:
            self._proc.kill()
            self._proc.wait(timeout=2)


def _windows_pty_command(command: str | list[str], shell: bool) -> list[str]:
    if shell:
        if isinstance(command, str):
            return ["cmd", "/C", command]
        return ["cmd", "/C", render_command_list(command)]
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


def _pty_command(command: str | list[str], shell: bool) -> list[str]:
    if sys.platform == "win32":
        return _windows_pty_command(command, shell)
    return _posix_pty_command(command, shell)


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
    with suppress(RuntimeError):
        native_apply_process_nice(pid, nice)


def _strip_wrapping_quotes(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    return value


def interactive_launch_spec(mode: InteractiveMode | str) -> InteractiveLaunchSpec:
    resolved = InteractiveMode(mode)
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
            creationflags=CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else None,
            restore_terminal=True,
        )
    return InteractiveLaunchSpec(
        mode=resolved,
        uses_pty=False,
        ctrl_c_owner="shared",
        creationflags=None,
        restore_terminal=False,
    )
