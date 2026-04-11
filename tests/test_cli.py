from __future__ import annotations

import io
from pathlib import Path

from running_process import cli


class _FakeProcess:
    def __init__(self, *, pid: int = 4321, returncode: int = 0) -> None:
        self.pid = pid
        self.returncode = returncode
        self.kill_called = False
        self.wait_calls = 0
        self.poll_calls = 0
        self.stdout = io.BytesIO()
        self.stderr = io.BytesIO()

    def poll(self) -> int | None:
        self.poll_calls += 1
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        self.wait_calls += 1
        return self.returncode

    def kill(self) -> None:
        self.kill_called = True


class _PollingProcess:
    def __init__(self, polls_before_exit: int) -> None:
        self.pid = 99
        self.returncode = 0
        self._polls_before_exit = polls_before_exit
        self.stdout = io.BytesIO(b"tick\n")
        self.stderr = io.BytesIO()

    def poll(self) -> int | None:
        if self._polls_before_exit > 0:
            self._polls_before_exit -= 1
            return None
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        return self.returncode


class _BufferedTextStream:
    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def flush(self) -> None:
        return None


class _Read1OnlyStream:
    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = list(chunks)
        self.closed = False

    def read1(self, size: int) -> bytes:
        del size
        if not self._chunks:
            return b""
        return self._chunks.pop(0)

    def read(self, size: int) -> bytes:
        del size
        raise AssertionError("read() should not be used when read1() is available")

    def close(self) -> None:
        self.closed = True


def test_normalize_command_strips_separator() -> None:
    assert cli._normalize_command(["--", "python", "-m", "ci.test"]) == [
        "python",
        "-m",
        "ci.test",
    ]


def test_cli_passes_in_running_process_to_child(monkeypatch) -> None:
    seen: dict[str, object] = {}
    fake_process = _FakeProcess(returncode=0)

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (0, False),
    )

    assert cli.main(["--", "python", "-m", "ci.test"]) == 0
    assert seen["command"] == ["python", "-m", "ci.test"]
    assert isinstance(seen["env"], dict)
    assert seen["env"][cli.IN_RUNNING_PROCESS_ENV] == cli.IN_RUNNING_PROCESS_VALUE, (
        "pytest must be run through the running-process CLI so it provides "
        "IN_RUNNING_PROCESS=running-process-cli and can auto-dump stacks on hangs; "
        "do not manually set IN_RUNNING_PROCESS in agent-driven test commands."
    )
    assert seen["stdout"] is cli.subprocess.PIPE
    assert seen["stderr"] is cli.subprocess.PIPE


def test_timeout_collects_diagnostics_and_kills_child(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess()
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "timeout.json"

    def fake_kill(child):
        seen["killed_child"] = child
        child.kill()

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (None, True),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)
    monkeypatch.setattr(cli, "_kill_supervised_process", fake_kill)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        timeout=1.5,
        stack_dump_dir=tmp_path,
    )

    assert code == cli.DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE
    assert fake_process.kill_called is True
    assert seen["killed_child"] is fake_process
    assert seen["dump"] == {
        "reason": "timeout",
        "command": ["python", "-m", "ci.test"],
        "pid": 4321,
        "returncode": None,
        "timeout_seconds": 1.5,
        "dump_dir": tmp_path,
    }


def test_timeout_can_disable_auto_stack_dumping(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess()
    dump_called = False
    killed = False

    def fake_popen(command, env, stdout, stderr):
        del command, env, stdout, stderr
        return fake_process

    def fake_dump(**kwargs):
        del kwargs
        nonlocal dump_called
        dump_called = True
        return tmp_path / "timeout.json"

    def fake_kill(child):
        nonlocal killed
        killed = True
        child.kill()

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (None, True),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)
    monkeypatch.setattr(cli, "_kill_supervised_process", fake_kill)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        timeout=1.5,
        auto_stack_dumping=False,
        stack_dump_dir=tmp_path,
    )

    assert code == cli.DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE
    assert dump_called is False
    assert killed is True


def test_abnormal_exit_collects_diagnostics(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess(returncode=3)
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "abnormal.json"

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (3, False),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        stack_dump_dir=tmp_path,
    )

    assert code == 3
    assert seen["dump"] == {
        "reason": "abnormal-exit",
        "command": ["python", "-m", "ci.test"],
        "pid": 4321,
        "returncode": 3,
        "timeout_seconds": None,
        "dump_dir": tmp_path,
    }


def test_wait_for_child_timeout_is_based_on_output_activity(monkeypatch) -> None:
    process = _PollingProcess(polls_before_exit=20)
    monotonic_values = iter([0.0, 0.0, 0.02, 0.04, 0.06, 0.08, 0.11, 0.13])
    monkeypatch.setattr(cli.time, "monotonic", lambda: next(monotonic_values))
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: None)
    stdout = _BufferedTextStream()
    stderr = _BufferedTextStream()
    monkeypatch.setattr(cli.sys, "stdout", stdout)
    monkeypatch.setattr(cli.sys, "stderr", stderr)

    returncode, timed_out = cli._wait_for_child_with_activity_timeout(process, timeout=0.1)

    assert timed_out is True
    assert returncode is None
    assert stdout.buffer.getvalue() == b"tick\n"
    assert stderr.buffer.getvalue() == b""


def test_wait_for_child_returns_after_exit_and_stream_drain(monkeypatch) -> None:
    process = _PollingProcess(polls_before_exit=1)
    call_count = [0]

    def fake_monotonic():
        val = call_count[0] * 0.02
        call_count[0] += 1
        return val

    monkeypatch.setattr(cli.time, "monotonic", fake_monotonic)
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: None)
    stdout = _BufferedTextStream()
    stderr = _BufferedTextStream()
    monkeypatch.setattr(cli.sys, "stdout", stdout)
    monkeypatch.setattr(cli.sys, "stderr", stderr)

    returncode, timed_out = cli._wait_for_child_with_activity_timeout(process, timeout=0.5)

    assert timed_out is False
    assert returncode == 0
    assert stdout.buffer.getvalue() == b"tick\n"


def test_stream_reader_prefers_read1_for_pipe_like_streams() -> None:
    source = _Read1OnlyStream([b"alpha", b"beta"])
    sink = _BufferedTextStream()
    touched = 0

    def touch() -> None:
        nonlocal touched
        touched += 1

    cli._stream_reader(source, sink, touch_activity=touch)

    assert sink.buffer.getvalue() == b"alphabeta"
    assert touched == 2
    assert source.closed is True


def test_dump_diagnostics_writes_metadata_and_py_spy_log(monkeypatch, tmp_path: Path) -> None:
    py_spy_log = tmp_path / "py-spy.log"
    native_log = tmp_path / "native.log"

    def fake_py_spy_dump(*, pid: int | None, log_path: Path) -> bool:
        assert pid == 77
        py_spy_log.write_text("py-spy output", encoding="utf-8")
        log_path.write_text("py-spy output", encoding="utf-8")
        return True

    def fake_native_dump(*, pid: int | None, log_path: Path) -> bool:
        assert pid == 77
        native_log.write_text("native output", encoding="utf-8")
        log_path.write_text("native output", encoding="utf-8")
        return True

    monkeypatch.setattr(cli, "_run_py_spy_dump", fake_py_spy_dump)
    monkeypatch.setattr(cli, "_run_native_debugger_dump", fake_native_dump)

    metadata_path = cli._dump_diagnostics(
        reason="timeout",
        command=["python", "-m", "ci.test"],
        pid=77,
        returncode=None,
        timeout_seconds=5.0,
        dump_dir=tmp_path,
    )

    assert metadata_path.is_file()
    assert metadata_path.suffix == ".json"
    metadata = metadata_path.read_text(encoding="utf-8")
    assert '"reason": "timeout"' in metadata
    assert '"pid": 77' in metadata
    assert any(path.name.endswith(".py-spy.log") for path in tmp_path.iterdir())
    assert any(path.name.endswith(".native-debugger.log") for path in tmp_path.iterdir())


def test_run_py_spy_dump_records_unavailable_tool(monkeypatch, tmp_path: Path) -> None:
    log_path = tmp_path / "dump.log"
    monkeypatch.setattr(cli.shutil, "which", lambda name: None)
    result = cli._run_py_spy_dump(pid=123, log_path=log_path)

    assert result is False
    assert "py-spy unavailable" in log_path.read_text(encoding="utf-8")


def test_native_debugger_command_prefers_lldb(monkeypatch) -> None:
    monkeypatch.setattr(
        cli.shutil,
        "which",
        lambda name: "C:/tools/lldb.exe" if name == "lldb" else "C:/tools/gdb.exe",
    )

    commands = cli._native_debugger_commands(123)

    assert commands
    assert commands[0][0] == "C:/tools/lldb.exe"
    assert "thread backtrace all" in commands[0]
    assert commands[1][0] == "C:/tools/gdb.exe"


def test_demangle_native_debugger_text_uses_cxxfilt(monkeypatch) -> None:
    seen: list[list[str]] = []

    def fake_run(command, **kwargs):
        seen.append(command)

        class Result:
            returncode = 0
            stdout = "running_process_core::NativeProcess::wait::h6b7a0fd0be7a0f11\n"

        return Result()

    monkeypatch.setattr(cli.shutil, "which", lambda name: "C:/tools/c++filt.exe")
    monkeypatch.setattr(cli.subprocess, "run", fake_run)

    rendered = cli._demangle_native_debugger_text(
        "frame _ZN20running_process_core13NativeProcess4wait17h6b7a0fd0be7a0f11E"
    )

    assert rendered == "frame running_process_core::NativeProcess::wait"
    assert seen == [["C:/tools/c++filt.exe"]]


def test_run_native_debugger_dump_falls_back_after_failed_debugger(
    monkeypatch, tmp_path: Path
) -> None:
    log_path = tmp_path / "native.log"
    monkeypatch.setattr(
        cli,
        "_native_debugger_commands",
        lambda pid: [["lldb", "--batch", str(pid)], ["gdb", "--batch", str(pid)]],
    )

    def fake_run(command, **kwargs):
        del kwargs

        class Result:
            def __init__(self, returncode: int, stdout: str) -> None:
                self.returncode = returncode
                self.stdout = stdout
                self.stderr = ""

        if command[0] == "lldb":
            return Result(1, "")
        return Result(
            0,
            "_ZN20running_process_core13NativeProcess4wait17h6b7a0fd0be7a0f11E\n",
        )

    monkeypatch.setattr(
        cli.shutil,
        "which",
        lambda name: "C:/tools/c++filt.exe" if name == "c++filt" else None,
    )

    def fake_cxxfilt(command, **kwargs):
        del kwargs
        if command == ["C:/tools/c++filt.exe"]:
            class Result:
                returncode = 0
                stdout = "running_process_core::NativeProcess::wait::h6b7a0fd0be7a0f11\n"
                stderr = ""

            return Result()
        return fake_run(command)

    monkeypatch.setattr(cli.subprocess, "run", fake_cxxfilt)

    assert cli._run_native_debugger_dump(pid=123, log_path=log_path) is True
    text = log_path.read_text(encoding="utf-8")
    assert "$ gdb --batch 123" in text
    assert "running_process_core::NativeProcess::wait" in text
    assert "_ZN20running_process_core13NativeProcess4wait17h6b7a0fd0be7a0f11E" not in text


def test_run_native_debugger_dump_records_unavailable_tool(monkeypatch, tmp_path: Path) -> None:
    log_path = tmp_path / "native.log"
    monkeypatch.setattr(cli, "_native_debugger_commands", lambda pid: [])

    result = cli._run_native_debugger_dump(pid=123, log_path=log_path)

    assert result is False
    assert "native debugger unavailable" in log_path.read_text(encoding="utf-8")
