from __future__ import annotations

import os
import re
import shlex
import sys
import time
from collections.abc import Callable
from contextlib import suppress
from dataclasses import dataclass
from io import TextIOBase
from pathlib import Path
from typing import Any, ClassVar, NamedTuple

from running_process._native import NativeProcess
from running_process.command_render import list2cmdline
from running_process.compat import (
    DEVNULL,
    PIPE,
    STDOUT,
    CalledProcessError,
    CompletedProcess,
    TimeoutExpired,
    make_completed_process,
)
from running_process.exit_status import ExitStatus, ProcessAbnormalExit, classify_exit_status
from running_process.expect import (
    ExpectAction,
    ExpectMatch,
    ExpectPattern,
    ExpectRule,
    apply_expect_action,
)
from running_process.line_iterator import _RunningProcessLineIterator
from running_process.priority import CpuPriority, normalize_nice
from running_process.pty import (
    Expect,
    IdleDetector,
    IdleWaitResult,
    InteractiveLaunchSpec,
    InteractiveMode,
    InteractiveProcess,
    PseudoTerminalProcess,
    SignalBool,
    WaitCheckpoint,
    WaitCondition,
    WaitForResult,
    interactive_launch_spec,
)
from running_process.running_process_manager import RunningProcessManagerSingleton

_BUFSIZE_NOT_SET = object()


class EndOfStream:
    def __repr__(self) -> str:
        return "EOS"


EOS = EndOfStream()
_FINALIZER_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)


@dataclass
class ProcessInfo:
    pid: int
    command: str | list[str]
    duration: float


EchoValue = str | bytes


def _is_eos(value: object) -> bool:
    return isinstance(value, EndOfStream)


class ProcessOutputEvent(NamedTuple):
    stdout: EchoValue | EndOfStream | None
    stderr: EchoValue | EndOfStream | None
    exit_code: int | None

    @property
    def streams_drained(self) -> bool:
        return (self.stdout is None or _is_eos(self.stdout)) and (
            self.stderr is None or _is_eos(self.stderr)
        )

    @property
    def finished_and_drained(self) -> bool:
        return self.streams_drained and self.exit_code is not None


class _RunningProcessOutputIterator:
    def __init__(self, process: RunningProcess, timeout: float | None) -> None:
        self._process = process
        self._timeout = timeout
        self._streams_drained = False
        self._finished = False

    def __iter__(self) -> _RunningProcessOutputIterator:
        return self

    def __next__(self) -> ProcessOutputEvent:
        if self._finished:
            raise StopIteration
        if self._process._pty_process is not None:
            raise NotImplementedError(
                "stdout/stderr tuple iteration is only available for pipe-backed RunningProcess"
            )
        if not self._process.capture:
            raise NotImplementedError("stdout/stderr tuple iteration requires capture=True")
        if self._streams_drained:
            exit_code = self._process.poll()
            if exit_code is None:
                exit_code = self._process._proc.wait(timeout=self._timeout)
                self._process._end_time = self._process._end_time or time.time()
                RunningProcessManagerSingleton.unregister(self._process)
            self._finished = True
            return ProcessOutputEvent(EOS, EOS, exit_code)

        status, stream, line = self._process._proc.take_combined_line(self._timeout)
        exit_code = self._process.poll()
        if status == "timeout":
            raise TimeoutError("No stdout or stderr available before timeout")
        if status == "line" and stream is not None and line is not None:
            if stream == "stdout":
                return ProcessOutputEvent(self._process._format(line), None, exit_code)
            return ProcessOutputEvent(None, self._process._format(line), exit_code)

        self._streams_drained = True
        if exit_code is None:
            try:
                grace_timeout = 0.01
                if self._timeout is not None:
                    grace_timeout = min(self._timeout, grace_timeout)
                exit_code = self._process._proc.wait(timeout=grace_timeout)
                self._process._end_time = self._process._end_time or time.time()
                RunningProcessManagerSingleton.unregister(self._process)
            except TimeoutError:
                exit_code = None
        if exit_code is not None:
            self._finished = True
        return ProcessOutputEvent(EOS, EOS, exit_code)


class CapturedProcessStream:
    def __init__(self, process: RunningProcess, stream: str) -> None:
        self._process = process
        self._stream = stream

    def available(self) -> bool:
        if self._stream == "combined":
            return self._process.has_pending_output()
        if self._stream == "stdout":
            return self._process.has_pending_stdout()
        return self._process.has_pending_stderr()

    def read(self) -> str | bytes:
        return self._process._captured_stream_value(self._stream)

    def drain(self) -> list[EchoValue] | list[tuple[str, EchoValue]]:
        if self._stream == "stdout":
            return self._process.drain_stdout()
        if self._stream == "stderr":
            return self._process.drain_stderr()
        return self._process.drain_combined()

    def __repr__(self) -> str:
        return repr(self.read())

    def __str__(self) -> str:
        value = self.read()
        if isinstance(value, str):
            return value
        return value.decode(self._process.encoding, self._process.errors)

    def __bytes__(self) -> bytes:
        value = self.read()
        if isinstance(value, bytes):
            return value
        return value.encode(self._process.encoding, self._process.errors)

    def __eq__(self, other: object) -> bool:
        return self.read() == other

    def __bool__(self) -> bool:
        return bool(self.read())

    def __len__(self) -> int:
        return len(self.read())

    def __contains__(self, item: object) -> bool:
        value = self.read()
        return item in value  # type: ignore[operator]

    def __getattr__(self, name: str) -> Any:
        return getattr(self.read(), name)


def _safe_console_write(stream: TextIOBase, line: EchoValue) -> None:
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


def _stdin_mode(stdin: int | Any | None, has_input: bool) -> str:
    if has_input:
        return "piped"
    if stdin is None:
        return "inherit"
    if stdin is DEVNULL:
        return "null"
    if stdin is PIPE:
        return "piped"
    raise ValueError("unsupported stdin value for RunningProcess; use None, PIPE, or DEVNULL")


def _validate_echo_flag(echo: bool) -> None:
    if not isinstance(echo, bool):
        raise TypeError(f"echo must be bool, got {type(echo).__name__}")


def _parse_shebang_command(script_path: Path) -> list[str]:
    first_line = script_path.read_text(encoding="utf-8", errors="replace").splitlines()[0]
    if first_line.startswith("\ufeff"):
        first_line = first_line.removeprefix("\ufeff")
    if not first_line.startswith("#!"):
        raise ValueError(f"Script does not start with a shebang: {script_path}")

    parts = shlex.split(first_line[2:].strip(), posix=True)
    if not parts:
        raise ValueError(f"Invalid shebang in script: {script_path}")

    interpreter = parts[0]
    if ("/" in interpreter or "\\" in interpreter) and not Path(interpreter).exists():
        parts[0] = Path(interpreter).name

    if Path(parts[0]).name == "env":
        env_args = parts[1:]
        if env_args and env_args[0] in {"-S", "--split-string"}:
            env_args = env_args[1:]
        if not env_args:
            raise ValueError(f"Shebang env launcher has no command: {script_path}")
        parts = env_args

    return parts


def _validate_expect_stream(stream: str) -> str:
    if stream not in {"stdout", "stderr", "combined"}:
        raise ValueError("stream must be 'stdout', 'stderr', or 'combined'")
    return stream


def _expect_pattern_spec(pattern: ExpectPattern) -> tuple[str, bool]:
    if isinstance(pattern, str):
        return pattern, False
    if isinstance(pattern, re.Pattern):
        return pattern.pattern, True
    raise TypeError("pattern must be a string or compiled regex pattern")


class RunningProcess:
    KEYBOARD_INTERRUPT_EXIT_CODES: ClassVar[set[int]] = {
        -2,
        -11,
        130,
        255,
        -1073741510,
        3221225786,
        4294967294,
    }
    SignalBool = SignalBool
    end_of_stream_type = EndOfStream

    def __init__(
        self,
        command: str | list[str],
        cwd: Path | None = None,
        check: bool = False,
        auto_run: bool = True,
        shell: bool | None = None,
        timeout: int | None = None,
        on_timeout: Callable[[ProcessInfo], None] | None = None,
        use_pty: bool = False,
        env: dict[str, str] | None = None,
        creationflags: int | None = None,
        capture: bool | None = None,
        stdin: int | Any | None = None,
        nice: int | CpuPriority | None = None,
        text: bool = True,
        encoding: str | None = None,
        errors: str | None = None,
        universal_newlines: bool = False,
        relay_terminal_input: bool = False,
        arm_idle_timeout_on_submit: bool = False,
        stderr: int | Any | None = None,
        **_popen_kwargs: Any,
    ) -> None:
        if isinstance(command, str) and shell is False:
            raise ValueError(
                "String commands require shell=True. "
                "Use shell=True or provide command as list[str]."
            )
        if shell is None:
            shell = isinstance(command, str)
        self._pty_process: PseudoTerminalProcess | None = None
        self.command = command
        self.shell = shell
        self.cwd = cwd
        self.check = check
        self.timeout = timeout
        self.on_timeout = on_timeout
        self.use_pty = use_pty
        self.env = env.copy() if env is not None else os.environ.copy()
        self.env.setdefault("PYTHONUTF8", "1")
        self.env.setdefault("PYTHONUNBUFFERED", "1")
        self.creationflags = creationflags
        self.capture = bool(capture) if capture is not None else True
        self.stdin = stdin
        self.nice = normalize_nice(nice)
        requested_text = text or universal_newlines
        self.text = requested_text
        self.encoding = encoding or "utf-8"
        self.errors = errors or "replace"
        self.relay_terminal_input = bool(relay_terminal_input)
        self.arm_idle_timeout_on_submit = bool(arm_idle_timeout_on_submit)
        if stderr not in (None, PIPE, STDOUT):
            raise ValueError("stderr must be None, PIPE, or STDOUT")
        if capture is False and stderr is PIPE:
            raise ValueError("stderr=PIPE requires capture=True")
        self._stderr_mode_name = "pipe" if stderr is PIPE else "stdout"
        if use_pty:
            self.capture = bool(capture) if capture is not None else False
            if stdin not in (None, PIPE):
                raise ValueError("use_pty=True only supports stdin=None or PIPE")
            if stderr is PIPE:
                raise ValueError("use_pty=True only supports stderr=None or STDOUT")
            self._pty_process = PseudoTerminalProcess(
                command,
                cwd=cwd,
                shell=self.shell,
                env=self.env,
                text=requested_text,
                encoding=self.encoding,
                errors=self.errors,
                nice=self.nice,
                capture=self.capture,
                relay_terminal_input=self.relay_terminal_input,
                arm_idle_timeout_on_submit=self.arm_idle_timeout_on_submit,
                auto_run=False,
            )
            self.text = False
            self._proc = None
        else:
            if self.relay_terminal_input:
                raise ValueError("relay_terminal_input requires use_pty=True")
            if self.arm_idle_timeout_on_submit:
                raise ValueError("arm_idle_timeout_on_submit requires use_pty=True")
            self._proc = NativeProcess(
                command,
                cwd=str(cwd) if cwd is not None else None,
                shell=self.shell,
                capture=self.capture,
                env=self.env,
                creationflags=creationflags,
                text=self.text,
                encoding=self.encoding if self.text else None,
                errors=self.errors if self.text else None,
                stdin_mode_name=_stdin_mode(stdin, has_input=False),
                stderr_mode_name=self._stderr_mode_name,
                nice=self.nice,
            )
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._exit_status: ExitStatus | None = None
        if auto_run:
            self.start()

    def _format(self, line: EchoValue) -> EchoValue:
        return line

    def _create_process_info(self) -> ProcessInfo:
        return ProcessInfo(
            pid=self.pid or 0,
            command=self.command,
            duration=(time.time() - self._start_time) if self._start_time is not None else 0.0,
        )

    def get_command_str(self) -> str:
        if isinstance(self.command, list):
            return list2cmdline(self.command)
        return self.command

    def start(self) -> None:
        if self._pty_process is not None:
            self._pty_process.start()
        else:
            self._proc.start()
        self._start_time = time.time()
        RunningProcessManagerSingleton.register(self)

    def _handle_timeout(self, timeout: float) -> None:
        if self.on_timeout is not None:
            self.on_timeout(self._create_process_info())
        self.kill()
        raise TimeoutError(f"Process timed out after {timeout} seconds: {self.get_command_str()}")

    def get_next_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        if self._pty_process is not None:
            if not self.capture:
                raise NotImplementedError("PTY line reads require capture=True")
            try:
                return self._format(self._pty_process.read(timeout=timeout))
            except EOFError:
                return EOS
        status, _stream, line = self._proc.take_combined_line(timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No combined output available before timeout")
        return EOS

    def get_next_stdout_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        if self._pty_process is not None:
            if not self.capture:
                raise NotImplementedError("PTY stdout reads require capture=True")
            return self.get_next_line(timeout)
        status, line = self._proc.take_stream_line("stdout", timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No stdout available before timeout")
        return EOS

    def get_next_stderr_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        if self._pty_process is not None:
            if not self.capture:
                raise NotImplementedError("PTY stderr reads require capture=True")
            if self._pty_process.poll() is not None:
                return EOS
            raise TimeoutError("No stderr available before timeout")
        status, line = self._proc.take_stream_line("stderr", timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No stderr available before timeout")
        return EOS

    def get_next_line_non_blocking(self) -> EchoValue | None | EndOfStream:
        try:
            return self.get_next_line(timeout=0)
        except TimeoutError:
            return None

    def drain_stdout(self) -> list[EchoValue]:
        if self._pty_process is not None:
            if not self.capture:
                return []
            return [self._format(line) for line in self._pty_process.drain()]
        return [self._format(line) for line in self._proc.drain_stream("stdout")]

    def drain_stderr(self) -> list[EchoValue]:
        if self._pty_process is not None:
            return []
        return [self._format(line) for line in self._proc.drain_stream("stderr")]

    def drain_combined(self) -> list[tuple[str, EchoValue]]:
        if self._pty_process is not None:
            if not self.capture:
                return []
            return [("stdout", self._format(line)) for line in self._pty_process.drain()]
        return [(stream, self._format(line)) for stream, line in self._proc.drain_combined()]

    def has_pending_output(self) -> bool:
        if self._pty_process is not None:
            if not self.capture:
                self._pty_process.available()
                return False
            return self._pty_process.available()
        return self._proc.has_pending_combined()

    def has_pending_stdout(self) -> bool:
        if self._pty_process is not None:
            if not self.capture:
                self._pty_process.available()
                return False
            return self._pty_process.available()
        return self._proc.has_pending_stream("stdout")

    def has_pending_stderr(self) -> bool:
        if self._pty_process is not None:
            return False
        return self._proc.has_pending_stream("stderr")

    def poll(self) -> int | None:
        result = self._pty_process.poll() if self._pty_process is not None else self._proc.poll()
        if result is not None and self._end_time is None:
            self._end_time = time.time()
            RunningProcessManagerSingleton.unregister(self)
        return result

    def is_running(self) -> bool:
        return self.poll() is None

    def is_runninng(self) -> bool:
        return self.is_running()

    @property
    def idle_timeout_enabled(self) -> bool:
        if self._pty_process is None:
            raise AttributeError("idle_timeout_enabled is only available for PTY-backed processes")
        return self._pty_process.idle_timeout_enabled

    @idle_timeout_enabled.setter
    def idle_timeout_enabled(self, enabled: bool) -> None:
        if self._pty_process is None:
            raise AttributeError("idle_timeout_enabled is only available for PTY-backed processes")
        self._pty_process.idle_timeout_enabled = enabled

    @property
    def proc(self) -> NativeProcess | None:
        """The underlying process object, or None if start() hasn't been called.

        Backwards compatible with the pre-Rust API where proc was None
        until start() created a subprocess.Popen.
        """
        if self._start_time is None:
            return None
        return self._proc

    @property
    def is_started(self) -> bool:
        return self._start_time is not None

    @property
    def finished(self) -> bool:
        return self.returncode is not None

    def _echo_streams(self) -> None:
        for line in self.drain_stdout():
            _safe_console_write(sys.stdout, line)
        for line in self.drain_stderr():
            _safe_console_write(sys.stderr, line)

    def wait(
        self,
        echo: bool = False,
        timeout: float | None = None,
        *,
        raise_on_abnormal_exit: bool = False,
        idle_detector: IdleDetector = None,
    ) -> int | IdleWaitResult:
        _validate_echo_flag(echo)
        if idle_detector is not None:
            return self.wait_for_idle(
                idle_detector,
                echo=echo,
                timeout=timeout,
                raise_on_abnormal_exit=raise_on_abnormal_exit,
            )
        if self._pty_process is not None:
            effective_timeout = timeout if timeout is not None else self.timeout
            if not echo:
                code = self._pty_process.wait(
                    timeout=effective_timeout,
                    raise_on_abnormal_exit=raise_on_abnormal_exit,
                )
            else:
                deadline = (
                    time.time() + effective_timeout if effective_timeout is not None else None
                )
                while True:
                    code = self.poll()
                    if code is not None:
                        code = self._pty_process.wait(timeout=0)
                        break
                    if deadline is not None and time.time() >= deadline:
                        self._handle_timeout(effective_timeout)
                    self._pty_process._echo_to_console(sys.stdout)
                    time.sleep(0.01)
                self._pty_process._echo_to_console(sys.stdout)
            self._end_time = self._end_time or time.time()
            RunningProcessManagerSingleton.unregister(self)
            self._exit_status = classify_exit_status(code, self.KEYBOARD_INTERRUPT_EXIT_CODES)
            return code
        effective_timeout = timeout if timeout is not None else self.timeout
        deadline = time.time() + effective_timeout if effective_timeout is not None else None
        if not echo:
            try:
                code = self._proc.wait(timeout=effective_timeout)
            except TimeoutError:
                self._handle_timeout(effective_timeout)
        else:
            while True:
                code = self.poll()
                if code is not None:
                    code = self._proc.wait(timeout=0)
                    break
                if deadline is not None and time.time() >= deadline:
                    self._handle_timeout(effective_timeout)
                self._echo_streams()
                time.sleep(0.01)

        if echo:
            self._echo_streams()

        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)
        self._exit_status = classify_exit_status(code, self.KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in self.KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def wait_for_idle(
        self,
        idle_detector: IdleDetector | None = None,
        *,
        echo: bool = False,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
    ) -> IdleWaitResult:
        _validate_echo_flag(echo)
        if self._pty_process is None:
            raise NotImplementedError("idle detection currently only supports PTY-backed processes")

        effective_timeout = timeout if timeout is not None else self.timeout
        result = self._pty_process.wait_for_idle(
            idle_detector,
            timeout=effective_timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo,
        )
        if echo:
            self._pty_process._echo_to_console(sys.stdout)
        if result.returncode is not None:
            self._end_time = self._end_time or time.time()
            RunningProcessManagerSingleton.unregister(self)
            self._exit_status = classify_exit_status(
                result.returncode, self.KEYBOARD_INTERRUPT_EXIT_CODES
            )
        return result

    def wait_for_expect(
        self,
        next_expect: Expect | None = None,
        *,
        echo: bool = False,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
    ) -> WaitForResult:
        _validate_echo_flag(echo)
        if self._pty_process is None:
            raise NotImplementedError(
                "wait_for_expect currently only supports PTY-backed processes"
            )
        result = self._pty_process.wait_for_expect(
            next_expect,
            timeout=timeout if timeout is not None else self.timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo,
        )
        if echo:
            self._pty_process._echo_to_console(sys.stdout)
        if result.returncode is not None:
            self._end_time = self._end_time or time.time()
            RunningProcessManagerSingleton.unregister(self)
            self._exit_status = classify_exit_status(
                result.returncode, self.KEYBOARD_INTERRUPT_EXIT_CODES
            )
        return result

    def checkpoint(self) -> WaitCheckpoint:
        if self._pty_process is None:
            raise NotImplementedError("checkpoint currently only supports PTY-backed processes")
        return self._pty_process.checkpoint()

    def wait_for(
        self,
        *conditions: WaitCondition
        | Callable[..., object]
        | list[WaitCondition | Callable[..., object]]
        | tuple[WaitCondition | Callable[..., object], ...],
        echo: bool = False,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
    ) -> WaitForResult:
        _validate_echo_flag(echo)
        if self._pty_process is None:
            raise NotImplementedError("wait_for currently only supports PTY-backed processes")

        result = self._pty_process.wait_for(
            *conditions,
            timeout=timeout if timeout is not None else self.timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo,
        )
        if echo:
            self._pty_process._echo_to_console(sys.stdout)
        if result.returncode is not None:
            self._end_time = self._end_time or time.time()
            RunningProcessManagerSingleton.unregister(self)
            self._exit_status = classify_exit_status(
                result.returncode, self.KEYBOARD_INTERRUPT_EXIT_CODES
            )
        return result

    def kill(self) -> None:
        if self._pty_process is not None:
            self._pty_process.kill()
        else:
            self._proc.kill()
        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)

    def terminate(self) -> None:
        if self._pty_process is not None:
            self._pty_process.terminate()
        else:
            self._proc.terminate()
        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)

    def send_interrupt(self) -> None:
        if self._pty_process is not None:
            self._pty_process.send_interrupt()
            return
        self._proc.send_interrupt()

    @property
    def pid(self) -> int | None:
        return self._pty_process.pid if self._pty_process is not None else self._proc.pid

    @property
    def returncode(self) -> int | None:
        return self._pty_process.poll() if self._pty_process is not None else self._proc.returncode

    def close(self) -> None:
        if self._pty_process is not None:
            try:
                code = self.poll()
            except _FINALIZER_CLEANUP_ERRORS:
                code = None
            if code is not None:
                RunningProcessManagerSingleton.unregister(self)
                return
            with suppress(*_FINALIZER_CLEANUP_ERRORS):
                self.kill()
            return
        with suppress(*_FINALIZER_CLEANUP_ERRORS):
            self._proc.close()
        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)

    def __del__(self) -> None:
        with suppress(*_FINALIZER_CLEANUP_ERRORS):
            self.close()

    @property
    def exit_status(self) -> ExitStatus | None:
        if self.returncode is None:
            return None
        if self._exit_status is None:
            self._exit_status = classify_exit_status(
                self.returncode, self.KEYBOARD_INTERRUPT_EXIT_CODES
            )
        return self._exit_status

    @property
    def start_time(self) -> float | None:
        return self._start_time

    @property
    def end_time(self) -> float | None:
        return self._end_time

    @property
    def duration(self) -> float | None:
        if self._start_time is None or self._end_time is None:
            return None
        return self._end_time - self._start_time

    @property
    def stdout_stream(self) -> CapturedProcessStream:
        return CapturedProcessStream(self, "stdout")

    @property
    def stderr_stream(self) -> CapturedProcessStream:
        return CapturedProcessStream(self, "stderr")

    @property
    def combined_stream(self) -> CapturedProcessStream:
        return CapturedProcessStream(self, "combined")

    @property
    def stdout(self) -> str | bytes:
        return self._captured_stream_value("stdout")

    @property
    def stderr(self) -> str | bytes:
        return self._captured_stream_value("stderr")

    @property
    def combined_output(self) -> str | bytes:
        return self._captured_stream_value("combined")

    def _captured_stream_value(self, stream: str) -> str | bytes:
        if self._pty_process is not None:
            if stream == "stderr":
                return b""
            return self._pty_process.output
        if stream == "stdout":
            lines = self._proc.captured_stdout()
        elif stream == "stderr":
            lines = self._proc.captured_stderr()
        else:
            lines = [line for _stream, line in self._proc.captured_combined()]
        return "\n".join(lines) if self.text else b"\n".join(lines)

    def discard_captured_output(self, stream: str = "combined") -> int:
        stream = _validate_expect_stream(stream)
        if self._pty_process is not None:
            if stream == "stderr":
                return 0
            return self._pty_process.discard_output()
        if stream == "combined":
            return int(self._proc.clear_captured_combined())
        return int(self._proc.clear_captured_stream(stream))

    def captured_output_bytes(self, stream: str = "combined") -> int:
        stream = _validate_expect_stream(stream)
        if self._pty_process is not None:
            if stream == "stderr":
                return 0
            return self._pty_process.output_bytes
        if stream == "combined":
            return int(self._proc.captured_combined_bytes())
        return int(self._proc.captured_stream_bytes(stream))

    def line_iter(self, timeout: float | None) -> _RunningProcessLineIterator:
        return _RunningProcessLineIterator(self, timeout)

    def stream_iter(self, timeout: float | None = None) -> _RunningProcessOutputIterator:
        return _RunningProcessOutputIterator(self, timeout)

    def __iter__(self) -> _RunningProcessOutputIterator:
        return self.stream_iter()

    def write(self, data: str | bytes, *, submit: bool = False) -> None:
        if self._pty_process is not None:
            self._pty_process.write(data, submit=submit)
            return
        payload = data.encode(self.encoding, self.errors) if isinstance(data, str) else data
        self._proc.write_stdin(payload)

    def submit(self, data: str | bytes = "\n") -> None:
        self.write(data, submit=True)

    def expect(
        self,
        pattern: ExpectPattern,
        *,
        timeout: float | None = None,
        action: ExpectAction = None,
        stream: str = "combined",
    ) -> ExpectMatch:
        if self._pty_process is not None:
            if stream != "combined":
                raise ValueError("PTY compatibility mode only supports combined output")
            match = self._pty_process.expect(pattern, timeout=timeout, action=action)
            return match
        stream = _validate_expect_stream(stream)
        native_pattern, is_regex = _expect_pattern_spec(pattern)
        status, buffer, matched, start, end, groups = self._proc.expect(
            stream,
            native_pattern,
            is_regex=is_regex,
            timeout=timeout,
        )
        if status == "timeout":
            raise TimeoutError(f"Pattern not found before timeout: {pattern!r}")
        if status == "eof" or matched is None or start is None or end is None:
            raise EOFError(f"Pattern not found before stream closed: {pattern!r}")

        match = ExpectMatch(
            buffer=buffer,
            matched=matched,
            span=(start, end),
            groups=tuple(groups),
        )
        apply_expect_action(self, action, match)
        return match

    @staticmethod
    def run(
        args: str | list[str],
        *,
        bufsize: int | object = _BUFSIZE_NOT_SET,
        executable: str | None = None,
        input: str | bytes | None = None,
        stdin: int | Any | None = None,
        stdout: int | Any | None = None,
        stderr: int | Any | None = None,
        capture_output: bool = False,
        shell: bool = False,
        cwd: str | Path | None = None,
        timeout: int | float | None = None,
        check: bool = False,
        encoding: str | None = None,
        errors: str | None = None,
        text: bool = True,
        env: dict[str, str] | None = None,
        universal_newlines: bool = False,
        on_timeout: Callable[[ProcessInfo], None] | None = None,
        raise_on_abnormal_exit: bool = False,
        nice: int | CpuPriority | None = None,
        **_other_popen_kwargs: Any,
    ) -> CompletedProcess[Any]:
        if input is not None and stdin is not None:
            raise ValueError("stdin and input arguments may not both be used.")

        if executable is not None:
            raise NotImplementedError("RunningProcess.run does not support executable= yet")
        if stdout not in (None, PIPE):
            raise NotImplementedError("RunningProcess.run only supports stdout=None or PIPE")
        if stderr not in (None, PIPE, STDOUT):
            raise NotImplementedError(
                "RunningProcess.run only supports stderr=None, PIPE, or STDOUT"
            )
        if bufsize is not _BUFSIZE_NOT_SET and bufsize != 1:
            raise NotImplementedError(
                "RunningProcess.run only supports default buffering or bufsize=1"
            )
        if _other_popen_kwargs:
            unsupported = ", ".join(sorted(_other_popen_kwargs))
            raise NotImplementedError(
                f"RunningProcess.run does not support extra Popen kwargs: {unsupported}"
            )
        should_text = text or universal_newlines or encoding is not None or errors is not None
        effective_stdin = PIPE if input is not None and stdin is None else stdin
        proc = RunningProcess(
            args,
            cwd=Path(cwd) if cwd is not None else None,
            shell=shell,
            timeout=int(timeout) if timeout is not None else None,
            capture=capture_output or stdout is PIPE or stderr is PIPE,
            env=env,
            stdin=effective_stdin,
            text=should_text,
            encoding=encoding,
            errors=errors,
            universal_newlines=universal_newlines,
            on_timeout=on_timeout,
            nice=nice,
            stderr=stderr,
        )
        if input is not None:
            payload = (
                input.encode(encoding or "utf-8", errors or "replace")
                if isinstance(input, str)
                else input
            )
            proc._proc.write_stdin(payload)
        try:
            returncode = proc.wait(timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit)
        except TimeoutError as exc:
            raise TimeoutExpired(args, timeout) from exc

        merged_output = capture_output or stdout is PIPE
        stdout_value: Any
        stderr_value: Any
        if merged_output:
            stdout_value = proc.combined_output if stderr in (None, STDOUT) else proc.stdout
        else:
            stdout_value = None
        stderr_value = proc.stderr if stderr is PIPE else None

        result: CompletedProcess[Any] = make_completed_process(
            args=args,
            returncode=returncode,
            stdout=stdout_value,
            stderr=stderr_value,
        )
        if check and result.returncode != 0:
            raise CalledProcessError(
                result.returncode,
                args,
                output=result.stdout,
                stderr=result.stderr,
            )
        return result

    @staticmethod
    def exec_script(
        script: str | Path,
        *script_args: str,
        cwd: str | Path | None = None,
        timeout: int | float | None = None,
        check: bool = False,
        capture_output: bool = True,
        text: bool = True,
        env: dict[str, str] | None = None,
        nice: int | CpuPriority | None = None,
    ) -> CompletedProcess[Any]:
        script_path = Path(script)
        command = [*_parse_shebang_command(script_path), str(script_path), *script_args]
        effective_cwd = cwd
        if (
            effective_cwd is None
            and len(command) >= 3
            and command[0] == "uv"
            and command[1] == "run"
            and command[2] == "--script"
        ):
            effective_cwd = str(script_path.parent)
        return RunningProcess.run(
            command,
            cwd=effective_cwd,
            timeout=timeout,
            check=check,
            capture_output=capture_output,
            text=text,
            env=env,
            nice=nice,
        )

    @staticmethod
    def pseudo_terminal(
        command: str | list[str],
        *,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: dict[str, str] | None = None,
        capture: bool = True,
        text: bool = False,
        encoding: str = "utf-8",
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool = True,
        auto_run: bool = True,
        expect: list[ExpectRule | Expect] | None = None,
        expect_timeout: float | None = None,
        idle_detector: IdleDetector | None = None,
        relay_terminal_input: bool = False,
        arm_idle_timeout_on_submit: bool = False,
    ) -> PseudoTerminalProcess:
        registered_expect: list[Expect] = []
        bootstrap_expect: list[ExpectRule] = []
        if expect is not None:
            for rule in expect:
                if isinstance(rule, Expect):
                    registered_expect.append(rule)
                else:
                    bootstrap_expect.append(rule)
        process = PseudoTerminalProcess(
            command,
            cwd=cwd,
            shell=shell,
            env=env,
            capture=capture,
            text=text,
            encoding=encoding,
            errors=errors,
            rows=rows,
            cols=cols,
            nice=nice,
            restore_terminal=restore_terminal,
            expect=registered_expect or None,
            idle_detector=idle_detector,
            relay_terminal_input=relay_terminal_input,
            arm_idle_timeout_on_submit=arm_idle_timeout_on_submit,
            auto_run=auto_run,
        )
        if bootstrap_expect:
            for rule in bootstrap_expect:
                process.expect(rule.pattern, timeout=expect_timeout, action=rule.action)
        return process

    psuedo_terminal = pseudo_terminal

    @staticmethod
    def interactive_launch_spec(mode: InteractiveMode | str) -> InteractiveLaunchSpec:
        return interactive_launch_spec(mode)

    @staticmethod
    def interactive(
        command: str | list[str],
        *,
        mode: InteractiveMode | str = InteractiveMode.CONSOLE_SHARED,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: dict[str, str] | None = None,
        text: bool = False,
        encoding: str = "utf-8",
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool | None = None,
        auto_run: bool = True,
    ) -> InteractiveProcess | PseudoTerminalProcess:
        resolved_mode = InteractiveMode(mode)
        if resolved_mode is InteractiveMode.PSEUDO_TERMINAL:
            return RunningProcess.pseudo_terminal(
                command,
                cwd=cwd,
                shell=shell,
                env=env,
                text=text,
                encoding=encoding,
                errors=errors,
                rows=rows,
                cols=cols,
                nice=nice,
                restore_terminal=True if restore_terminal is None else restore_terminal,
                auto_run=auto_run,
            )
        return InteractiveProcess(
            command,
            mode=resolved_mode,
            cwd=cwd,
            shell=shell,
            env=env,
            nice=nice,
            restore_terminal=restore_terminal,
            auto_run=auto_run,
        )

    @classmethod
    def run_streaming(
        cls,
        cmd: list[str],
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout: float | None = None,
        nice: int | CpuPriority | None = None,
        stdout_callback: Callable[[str], None] | None = None,
    ) -> int:
        process = cls(
            command=cmd,
            cwd=Path(cwd) if cwd is not None else None,
            env=env,
            timeout=int(timeout) if timeout is not None else None,
            nice=nice,
            auto_run=True,
        )
        deadline = time.time() + timeout if timeout is not None else None

        while True:
            code = process.poll()
            if stdout_callback is not None:
                for line in process.drain_stdout():
                    text = (
                        line.decode("utf-8", errors="replace")
                        if isinstance(line, bytes)
                        else line
                    )
                    stdout_callback(text)
                for line in process.drain_stderr():
                    _safe_console_write(sys.stderr, line)
            else:
                process._echo_streams()
            if code is not None:
                return code
            if deadline is not None and time.time() >= deadline:
                process._handle_timeout(timeout)
            time.sleep(0.01)


def subprocess_run(
    command: str | list[str],
    cwd: Path | None,
    check: bool,
    timeout: int,
    on_timeout: Callable[[ProcessInfo], None] | None = None,
    nice: int | CpuPriority | None = None,
) -> CompletedProcess[str]:
    try:
        return RunningProcess.run(
            command,
            cwd=cwd,
            check=check,
            timeout=timeout,
            capture_output=True,
            on_timeout=on_timeout,
            nice=nice,
        )
    except TimeoutExpired as exc:
        raise RuntimeError(
            f"CRITICAL: Process timed out after {timeout} seconds: {command}"
        ) from exc
