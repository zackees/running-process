from __future__ import annotations

import os
import shlex
import signal
import subprocess
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from io import TextIOBase
from pathlib import Path
from typing import Any, ClassVar

from running_process._native import NativeRunningProcess
from running_process.exit_status import ExitStatus, ProcessAbnormalExit, classify_exit_status
from running_process.expect import (
    ExpectAction,
    ExpectMatch,
    ExpectPattern,
    ExpectRule,
    apply_expect_action,
    ensure_text,
    search_expect_pattern,
)
from running_process.line_iterator import _RunningProcessLineIterator
from running_process.output_formatter import NullOutputFormatter, OutputFormatter
from running_process.priority import CpuPriority, normalize_nice
from running_process.pty import (
    InteractiveLaunchSpec,
    InteractiveMode,
    InteractiveProcess,
    PseudoTerminalProcess,
    Pty,
    PtyNotAvailableError,
    interactive_launch_spec,
)
from running_process.running_process_manager import RunningProcessManagerSingleton

_BUFSIZE_NOT_SET = object()


class EndOfStream:
    pass


@dataclass
class ProcessInfo:
    pid: int
    command: str | list[str]
    duration: float


EchoValue = str | bytes
EchoCallback = Callable[[EchoValue], None]


class CapturedProcessStream:
    def __init__(self, process: RunningProcess, stream: str) -> None:
        self._process = process
        self._stream = stream

    def available(self) -> bool:
        if self._stream == "combined":
            return self._process.has_pending_output()
        return self._process.proc.has_pending_stream(self._stream)

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


def _normalize_echo_callback(echo: bool | EchoCallback) -> EchoCallback:
    if echo is True:
        return lambda line: _safe_console_write(sys.stdout, line)
    if echo is False:
        return lambda _line: None
    if callable(echo):
        return echo
    raise TypeError(f"echo must be bool or callable, got {type(echo).__name__}")


def _stdin_mode(stdin: int | Any | None, has_input: bool) -> str:
    if has_input:
        return "piped"
    if stdin is None:
        return "inherit"
    if stdin is subprocess.DEVNULL:
        return "null"
    if stdin is subprocess.PIPE:
        return "piped"
    raise ValueError("unsupported stdin value for RunningProcess; use None, PIPE, or DEVNULL")


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
        on_complete: Callable[[], None] | None = None,
        output_formatter: OutputFormatter | None = None,
        use_pty: bool = False,
        env: dict[str, str] | None = None,
        creationflags: int | None = None,
        capture: bool = True,
        stdin: int | Any | None = None,
        nice: int | CpuPriority | None = None,
        text: bool = True,
        encoding: str | None = None,
        errors: str | None = None,
        universal_newlines: bool = False,
        **_popen_kwargs: Any,
    ) -> None:
        if isinstance(command, str) and shell is False:
            raise ValueError(
                "String commands require shell=True. "
                "Use shell=True or provide command as list[str]."
            )
        if shell is None:
            shell = isinstance(command, str)
        if use_pty:
            raise PtyNotAvailableError(
                "RunningProcess is pipe-backed. "
                "Use RunningProcess.pseudo_terminal(...) for PTY sessions."
            )
        self.command = command
        self.shell = shell
        self.cwd = cwd
        self.check = check
        self.timeout = timeout
        self.on_timeout = on_timeout
        self.on_complete = on_complete
        self.output_formatter = output_formatter or NullOutputFormatter()
        self.use_pty = False
        self.env = env.copy() if env is not None else os.environ.copy()
        self.env.setdefault("PYTHONUTF8", "1")
        self.env.setdefault("PYTHONUNBUFFERED", "1")
        self.creationflags = creationflags
        self.capture = capture
        self.stdin = stdin
        self.nice = normalize_nice(nice)
        self.text = text or universal_newlines
        self.encoding = encoding or "utf-8"
        self.errors = errors or "replace"
        self.proc = NativeRunningProcess(
            command,
            cwd=str(cwd) if cwd is not None else None,
            shell=self.shell,
            capture=capture,
            env=self.env,
            creationflags=creationflags,
            text=self.text,
            encoding=self.encoding if self.text else None,
            errors=self.errors if self.text else None,
            stdin_mode_name=_stdin_mode(stdin, has_input=False),
            nice=self.nice,
        )
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._formatter_started = False
        self._exit_status: ExitStatus | None = None
        if auto_run:
            self.start()

    def _ensure_formatter_started(self) -> None:
        if not self._formatter_started:
            self.output_formatter.begin()
            self._formatter_started = True

    def _finalize_formatter(self) -> None:
        if self._formatter_started:
            self.output_formatter.end()
            self._formatter_started = False

    def _format(self, line: EchoValue) -> EchoValue:
        if isinstance(line, bytes):
            return line
        self._ensure_formatter_started()
        return self.output_formatter.transform(line)

    def _pty_available(self) -> bool:
        return Pty.is_available()

    def _create_process_info(self) -> ProcessInfo:
        return ProcessInfo(
            pid=self.pid or 0,
            command=self.command,
            duration=(time.time() - self._start_time) if self._start_time is not None else 0.0,
        )

    def get_command_str(self) -> str:
        if isinstance(self.command, list):
            return subprocess.list2cmdline(self.command)
        return self.command

    def start(self) -> None:
        self.proc.start()
        self._start_time = time.time()
        RunningProcessManagerSingleton.register(self)

    def _handle_timeout(self, timeout: float) -> None:
        if self.on_timeout is not None:
            self.on_timeout(self._create_process_info())
        self.kill()
        raise TimeoutError(f"Process timed out after {timeout} seconds: {self.get_command_str()}")

    def get_next_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        status, _stream, line = self.proc.take_combined_line(timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No combined output available before timeout")
        return EndOfStream()

    def get_next_stdout_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        status, line = self.proc.take_stream_line("stdout", timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No stdout available before timeout")
        return EndOfStream()

    def get_next_stderr_line(self, timeout: float | None = None) -> EchoValue | EndOfStream:
        status, line = self.proc.take_stream_line("stderr", timeout)
        if status == "line" and line is not None:
            return self._format(line)
        if status == "timeout":
            raise TimeoutError("No stderr available before timeout")
        return EndOfStream()

    def get_next_line_non_blocking(self) -> EchoValue | None | EndOfStream:
        try:
            return self.get_next_line(timeout=0)
        except TimeoutError:
            return None

    def drain_stdout(self) -> list[EchoValue]:
        return [self._format(line) for line in self.proc.drain_stream("stdout")]

    def drain_stderr(self) -> list[EchoValue]:
        return [self._format(line) for line in self.proc.drain_stream("stderr")]

    def drain_combined(self) -> list[tuple[str, EchoValue]]:
        return [(stream, self._format(line)) for stream, line in self.proc.drain_combined()]

    def has_pending_output(self) -> bool:
        return self.proc.has_pending_combined()

    def has_pending_stdout(self) -> bool:
        return self.proc.has_pending_stream("stdout")

    def has_pending_stderr(self) -> bool:
        return self.proc.has_pending_stream("stderr")

    def poll(self) -> int | None:
        result = self.proc.poll()
        if result is not None and self._end_time is None:
            self._end_time = time.time()
            RunningProcessManagerSingleton.unregister(self)
        return result

    @property
    def finished(self) -> bool:
        return self.returncode is not None

    def _echo_streams(
        self,
        stdout_callback: EchoCallback,
        stderr_callback: EchoCallback | None = None,
    ) -> None:
        for line in self.drain_stdout():
            stdout_callback(line)
        if stderr_callback is not None:
            for line in self.drain_stderr():
                stderr_callback(line)

    def wait(
        self,
        echo: bool | EchoCallback = False,
        timeout: float | None = None,
        *,
        raise_on_abnormal_exit: bool = False,
    ) -> int:
        callback = _normalize_echo_callback(echo)
        effective_timeout = timeout if timeout is not None else self.timeout
        deadline = time.time() + effective_timeout if effective_timeout is not None else None

        while True:
            code = self.poll()
            if code is not None:
                code = self.proc.wait(timeout=0)
                break
            if deadline is not None and time.time() >= deadline:
                self._handle_timeout(effective_timeout)
            if echo is not False:
                self._echo_streams(callback, callback)
            time.sleep(0.01)

        if echo is not False:
            self._echo_streams(callback, callback)

        self._end_time = self._end_time or time.time()
        self._finalize_formatter()
        RunningProcessManagerSingleton.unregister(self)
        if self.on_complete is not None:
            self.on_complete()
        self._exit_status = classify_exit_status(code, self.KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in self.KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def kill(self) -> None:
        self.proc.kill()
        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)
        self._finalize_formatter()

    def terminate(self) -> None:
        self.proc.terminate()
        self._end_time = self._end_time or time.time()
        RunningProcessManagerSingleton.unregister(self)
        self._finalize_formatter()

    def send_interrupt(self) -> None:
        pid = self.pid
        if pid is None:
            raise RuntimeError("Process is not running.")
        if sys.platform == "win32":
            create_new_process_group = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
            has_process_group = bool(create_new_process_group) and bool(
                (self.creationflags or 0) & create_new_process_group
            )
            if not has_process_group:
                raise RuntimeError(
                    "send_interrupt on Windows requires CREATE_NEW_PROCESS_GROUP"
                )
            os.kill(pid, signal.CTRL_BREAK_EVENT)
            return
        os.kill(pid, signal.SIGINT)

    @property
    def pid(self) -> int | None:
        return self.proc.pid

    @property
    def returncode(self) -> int | None:
        return self.proc.returncode

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
        if stream == "stdout":
            lines = self.proc.captured_stdout()
        elif stream == "stderr":
            lines = self.proc.captured_stderr()
        else:
            lines = [line for _stream, line in self.proc.captured_combined()]
        return "\n".join(lines) if self.text else b"\n".join(lines)

    def line_iter(self, timeout: float | None) -> _RunningProcessLineIterator:
        return _RunningProcessLineIterator(self, timeout)

    def write(self, data: str | bytes) -> None:
        payload = (
            data.encode(self.encoding, self.errors) if isinstance(data, str) else data
        )
        self.proc.write_stdin(payload)

    def expect(
        self,
        pattern: ExpectPattern,
        *,
        timeout: float | None = None,
        action: ExpectAction = None,
        stream: str = "combined",
    ) -> ExpectMatch:
        stream = _validate_expect_stream(stream)
        deadline = time.time() + timeout if timeout is not None else None
        buffer = ensure_text(self._captured_stream_value(stream), self.encoding, self.errors)

        while True:
            match = search_expect_pattern(buffer, pattern)
            if match is not None:
                apply_expect_action(self, action, match)
                return match

            wait_timeout = 0.1
            if deadline is not None:
                remaining = deadline - time.time()
                if remaining <= 0:
                    raise TimeoutError(f"Pattern not found before timeout: {pattern!r}")
                wait_timeout = min(wait_timeout, remaining)

            if stream == "stdout":
                try:
                    item = self.get_next_stdout_line(timeout=wait_timeout)
                except TimeoutError:
                    continue
            elif stream == "stderr":
                try:
                    item = self.get_next_stderr_line(timeout=wait_timeout)
                except TimeoutError:
                    continue
            else:
                try:
                    item = self.get_next_line(timeout=wait_timeout)
                except TimeoutError:
                    continue
            if isinstance(item, EndOfStream):
                raise EOFError(f"Pattern not found before stream closed: {pattern!r}")
            buffer = f"{buffer}{ensure_text(item, self.encoding, self.errors)}\n"

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
    ) -> subprocess.CompletedProcess[Any]:
        if input is not None and stdin is not None:
            raise ValueError("stdin and input arguments may not both be used.")

        if executable is not None:
            raise NotImplementedError("RunningProcess.run does not support executable= yet")
        if stdout not in (None, subprocess.PIPE):
            raise NotImplementedError("RunningProcess.run only supports stdout=None or PIPE")
        if stderr not in (None, subprocess.PIPE):
            raise NotImplementedError("RunningProcess.run only supports stderr=None or PIPE")
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
        effective_stdin = subprocess.PIPE if input is not None and stdin is None else stdin
        proc = RunningProcess(
            args,
            cwd=Path(cwd) if cwd is not None else None,
            shell=shell,
            timeout=int(timeout) if timeout is not None else None,
            capture=capture_output or stdout is subprocess.PIPE or stderr is subprocess.PIPE,
            env=env,
            stdin=effective_stdin,
            text=should_text,
            encoding=encoding,
            errors=errors,
            universal_newlines=universal_newlines,
            on_timeout=on_timeout,
            nice=nice,
        )
        if input is not None:
            payload = (
                input.encode(encoding or "utf-8", errors or "replace")
                if isinstance(input, str)
                else input
            )
            proc.proc.write_stdin(payload)
        try:
            returncode = proc.wait(timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit)
        except TimeoutError as exc:
            raise subprocess.TimeoutExpired(args, timeout) from exc

        result: subprocess.CompletedProcess[Any] = subprocess.CompletedProcess(
            args=args,
            returncode=returncode,
            stdout=proc.stdout if (capture_output or stdout is subprocess.PIPE) else None,
            stderr=proc.stderr if (capture_output or stderr is subprocess.PIPE) else None,
        )
        if check and result.returncode != 0:
            raise subprocess.CalledProcessError(
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
    ) -> subprocess.CompletedProcess[Any]:
        script_path = Path(script)
        command = [*_parse_shebang_command(script_path), str(script_path), *script_args]
        return RunningProcess.run(
            command,
            cwd=cwd,
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
        text: bool = False,
        encoding: str = "utf-8",
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool = True,
        restore_callback: Callable[[], None] | None = None,
        cleanup_callback: Callable[[str], None] | None = None,
        auto_run: bool = True,
        expect: list[ExpectRule] | None = None,
        expect_timeout: float | None = None,
    ) -> PseudoTerminalProcess:
        process = PseudoTerminalProcess(
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
            restore_terminal=restore_terminal,
            restore_callback=restore_callback,
            cleanup_callback=cleanup_callback,
            auto_run=auto_run,
        )
        if expect is not None:
            for rule in expect:
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
        restore_callback: Callable[[], None] | None = None,
        cleanup_callback: Callable[[str], None] | None = None,
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
                restore_callback=restore_callback,
                cleanup_callback=cleanup_callback,
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
            restore_callback=restore_callback,
            cleanup_callback=cleanup_callback,
            auto_run=auto_run,
        )

    @classmethod
    def run_streaming(
        cls,
        cmd: list[str],
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout: float | None = None,
        stdout_callback: Callable[[str], None] | None = None,
        stderr_callback: Callable[[str], None] | None = None,
        nice: int | CpuPriority | None = None,
        **_kwargs: Any,
    ) -> int:
        process = cls(
            command=cmd,
            cwd=Path(cwd) if cwd is not None else None,
            env=env,
            timeout=int(timeout) if timeout is not None else None,
            nice=nice,
            auto_run=True,
        )
        stdout_callback = stdout_callback or print
        stderr_callback = stderr_callback or print
        deadline = time.time() + timeout if timeout is not None else None

        while True:
            code = process.poll()
            process._echo_streams(stdout_callback, stderr_callback)
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
) -> subprocess.CompletedProcess[str]:
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
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"CRITICAL: Process timed out after {timeout} seconds: {command}"
        ) from exc
