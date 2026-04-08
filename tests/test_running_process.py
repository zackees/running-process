from __future__ import annotations

import contextlib
import gc
import io
import os
import re
import subprocess
import sys
import tempfile
import time
import warnings
from pathlib import Path

import psutil
import pytest

from running_process import (
    CalledProcessError,
    CompletedProcess,
    CpuPriority,
    DEVNULL,
    EndOfStream,
    PIPE,
    ProcessAbnormalExit,
    ProcessInfo,
    RunningProcess,
    RunningProcessManagerSingleton,
    TimeoutExpired,
    cleanup_tracked_processes,
    list_tracked_processes,
    subprocess_run,
    tracked_pid_db_path,
)
from running_process.exit_status import classify_exit_status
from running_process.process_utils import get_process_tree_info, kill_process_tree


def test_basic_stdout_capture() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    code = process.wait()

    assert code == 0
    assert process.start_time is not None
    assert process.end_time is not None
    assert process.duration is not None
    assert process.stdout.strip() == "hello"
    assert process.stderr == ""
    assert process.combined_output.strip() == "hello"


def test_split_stdout_and_stderr_capture() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script])
    code = process.wait()

    assert code == 0
    assert process.stdout.strip() == "out"
    assert process.stderr.strip() == "err"
    assert "out" in process.combined_output
    assert "err" in process.combined_output


def test_invalid_utf8_replaced_by_default_for_running_process() -> None:
    script = (
        "import sys; "
        "sys.stdout.buffer.write(b'bad:\\xff\\n'); "
        "sys.stderr.buffer.write(b'err:\\xfe\\n'); "
        "sys.stdout.flush(); sys.stderr.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script])
    process.wait()

    assert process.stdout == "bad:\ufffd"
    assert process.stderr == "err:\ufffd"


def test_crlf_is_normalized_but_bare_cr_is_preserved() -> None:
    script = (
        "import sys; "
        "sys.stdout.buffer.write(b'first\\r\\nsecond\\rthird\\n'); "
        "sys.stdout.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script])
    process.wait()

    assert process.stdout == "first\nsecond\rthird"


def test_get_next_line_preserves_combined_stream() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script])

    seen = []
    while True:
        item = process.get_next_line(timeout=5)
        if isinstance(item, EndOfStream):
            break
        seen.append(item)

    process.wait()
    assert "out" in seen
    assert "err" in seen


def test_stream_specific_reads_and_availability() -> None:
    script = (
        "import sys, time; "
        "print('out-1'); sys.stdout.flush(); "
        "time.sleep(0.2); "
        "print('err-1', file=sys.stderr); sys.stderr.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script])

    stdout_line = process.get_next_stdout_line(timeout=2)
    assert stdout_line == "out-1"
    assert process.has_pending_stdout() is False

    stderr_line = process.get_next_stderr_line(timeout=2)
    assert stderr_line == "err-1"
    assert process.has_pending_stderr() is False

    process.wait()


def test_stdout_and_stderr_stream_objects_report_availability() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "print('stdout-ready'); sys.stdout.flush(); "
                "time.sleep(0.2); "
                "print('stderr-ready', file=sys.stderr); sys.stderr.flush()"
            ),
        ]
    )

    stdout_line = process.get_next_stdout_line(timeout=2)
    assert stdout_line == "stdout-ready"
    assert process.stdout_stream.available() is False

    stderr_line = process.get_next_stderr_line(timeout=2)
    assert stderr_line == "stderr-ready"
    assert process.stderr_stream.available() is False
    assert process.wait() == 0


def test_line_iter_uses_combined_stream() -> None:
    process = RunningProcess([sys.executable, "-c", "print('a'); print('b')"])
    with process.line_iter(timeout=5) as lines:
        collected = list(lines)
    process.wait()
    assert collected == ["a", "b"]


def test_get_next_line_non_blocking_returns_none_without_output() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(0.2); print('late')"])
    assert process.get_next_line_non_blocking() is None
    process.wait()


def test_drain_combined_includes_stream_names() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
    )
    process.wait()
    drained = process.drain_combined()
    assert ("stdout", "out") in drained
    assert ("stderr", "err") in drained


def test_stream_values_are_plain_text_and_streams_are_separate() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello world')"])
    process.wait()
    assert process.stdout.strip() == "hello world"
    assert "hello" in process.stdout
    assert str(process.stdout) == "hello world"
    assert process.stdout_stream.read() == "hello world"


def test_discard_captured_output_releases_pipe_history_bytes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; print('alpha'); print('beta', file=sys.stderr)"]
    )
    process.wait()

    assert process.captured_output_bytes("stdout") == len("alpha")
    assert process.captured_output_bytes("stderr") == len("beta")
    assert process.captured_output_bytes("combined") == len("alpha") + len("beta")
    assert process.discard_captured_output("stdout") == len("alpha")
    assert process.stdout == ""
    assert process.captured_output_bytes("stdout") == 0
    assert process.stderr == "beta"
    assert process.discard_captured_output("combined") == len("alpha") + len("beta")
    assert process.combined_output == ""
    assert process.captured_output_bytes("combined") == 0


def test_run_rejects_unsupported_subprocess_kwargs() -> None:
    with pytest.raises(NotImplementedError, match="executable="):
        RunningProcess.run([sys.executable, "-c", "print('x')"], executable=sys.executable)
    with pytest.raises(NotImplementedError, match="stdout=None or PIPE"):
        RunningProcess.run([sys.executable, "-c", "print('x')"], stdout=subprocess.DEVNULL)
    with pytest.raises(NotImplementedError, match="extra Popen kwargs"):
        RunningProcess.run([sys.executable, "-c", "print('x')"], start_new_session=True)


def test_run_can_raise_on_abnormal_exit() -> None:
    with pytest.raises(ProcessAbnormalExit) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import sys; sys.exit(3)"],
            capture_output=True,
            raise_on_abnormal_exit=True,
        )
    assert exc_info.value.status.returncode == 3
    assert exc_info.value.status.abnormal is True


def test_expect_matches_string_and_writes_to_stdin() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "print('prompt>'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "print('echo:' + line)"
            ),
        ],
        stdin=subprocess.PIPE,
    )
    match = process.expect("prompt>", timeout=5, action="typed text\n")
    assert match.matched == "prompt>"
    process.expect("echo:typed text", timeout=5)
    assert process.wait() == 0


def test_expect_matches_regex_groups() -> None:
    process = RunningProcess([sys.executable, "-c", "print('value=42')"])
    match = process.expect(re.compile(r"value=(\d+)"), timeout=5)
    assert match.groups == ("42",)
    process.wait()


def test_expect_rejects_invalid_stream_name() -> None:
    process = RunningProcess([sys.executable, "-c", "print('value=42')"])
    with pytest.raises(ValueError, match="stream must be"):
        process.expect("value", stream="bad")
    process.wait()


def test_timeout_kills_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"], timeout=1)
    with pytest.raises(TimeoutError):
        process.wait(timeout=1)
    assert process.finished


def test_running_process_registers_pid_in_sqlite_db() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    tracked = list_tracked_processes()

    assert tracked_pid_db_path().name == "tracked-pids.sqlite3"
    assert any(entry.pid == process.pid and entry.kind == "running_process" for entry in tracked)

    process.kill()
    assert all(entry.pid != process.pid for entry in list_tracked_processes())


def test_cleanup_tracked_processes_kills_registered_processes() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])

    killed = cleanup_tracked_processes()

    assert any(entry.pid == process.pid for entry in killed)
    deadline = time.time() + 5
    while process.poll() is None and time.time() < deadline:
        time.sleep(0.05)
    assert process.poll() is not None
    assert all(entry.pid != process.pid for entry in list_tracked_processes())


def test_terminate_finishes_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    process.terminate()
    assert process.finished


def test_manager_unregisters_after_wait() -> None:
    before = len(RunningProcessManagerSingleton.list_active())
    RunningProcess.run([sys.executable, "-c", "print('manager')"], capture_output=True)
    after = len(RunningProcessManagerSingleton.list_active())
    assert before == after


def test_manager_does_not_hold_strong_reference_to_abandoned_process() -> None:
    before = len(RunningProcessManagerSingleton.list_active())
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    assert len(RunningProcessManagerSingleton.list_active()) == before + 1

    pid = process.pid
    del process
    gc.collect()

    deadline = time.time() + 3.0
    while time.time() < deadline:
        if len(RunningProcessManagerSingleton.list_active()) == before:
            break
        gc.collect()
        time.sleep(0.05)

    assert len(RunningProcessManagerSingleton.list_active()) == before
    if pid is not None:
        assert not psutil.pid_exists(pid)


def test_run_matches_subprocess_contract() -> None:
    result = RunningProcess.run(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        capture_output=True,
        check=True,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "out" in lines
    assert "err" in lines
    assert result.stderr is None
    assert isinstance(result, CompletedProcess)
    assert isinstance(result, subprocess.CompletedProcess)


def test_run_can_explicitly_request_split_stderr() -> None:
    result = RunningProcess.run(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        capture_output=True,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
    )
    assert result.stdout == "out"
    assert result.stderr == "err"


def test_run_without_capture_returns_none_streams() -> None:
    result = RunningProcess.run([sys.executable, "-c", "print('out')"])
    assert result.stdout is None
    assert result.stderr is None


def test_run_capture_output_defaults_to_bytes() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(b'bad:\\xff'); sys.stderr.buffer.write(b'err')",
        ],
        capture_output=True,
        text=False,
    )
    assert result.stdout is not None
    assert b"bad:\xff" in result.stdout
    assert b"err" in result.stdout
    assert result.stderr is None


def test_run_capture_output_text_mode_replaces_invalid_utf8() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.buffer.write(b'bad:\\xff'); "
                "sys.stderr.buffer.write(b'err:\\xfe')"
            ),
        ],
        capture_output=True,
        text=True,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "bad:\ufffd" in lines
    assert "err:\ufffd" in lines
    assert result.stderr is None


def test_run_preserves_bare_carriage_return_in_text_mode() -> None:
    result = RunningProcess.run(
        [sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'a\\r\\nb\\rc\\n')"],
        capture_output=True,
        text=True,
    )
    assert result.stdout == "a\nb\rc"


def test_run_timeout_raises_timeout_expired() -> None:
    with pytest.raises(TimeoutExpired) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            capture_output=True,
            timeout=0.2,
        )
    assert isinstance(exc_info.value, subprocess.TimeoutExpired)


def test_running_process_exports_subprocess_compat_constants() -> None:
    assert PIPE is subprocess.PIPE
    assert DEVNULL is subprocess.DEVNULL


def test_run_check_raises_compat_called_process_error() -> None:
    with pytest.raises(CalledProcessError) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import sys; sys.exit(9)"],
            capture_output=True,
            check=True,
        )
    assert isinstance(exc_info.value, subprocess.CalledProcessError)


def test_run_with_text_input_capture_output() -> None:
    script = (
        "import sys; "
        "data = sys.stdin.read(); "
        "print(data.upper()); "
        "print('err:' + data.lower(), file=sys.stderr)"
    )
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            script,
        ],
        input="Hello\n",
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "HELLO" in lines
    assert "err:hello" in lines
    assert result.stderr is None


def test_run_with_bytes_input_capture_output() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])",
        ],
        input=b"abc",
        capture_output=True,
        text=False,
    )
    assert result.stdout == b"cba"
    assert result.stderr is None


def test_running_process_binary_mode_returns_bytes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'abc\\xff')"],
        text=False,
    )
    process.wait()
    assert process.stdout == b"abc\xff"


def test_run_input_and_stdin_conflict_raises() -> None:
    with pytest.raises(ValueError, match="stdin and input arguments may not both be used"):
        RunningProcess.run(
            [sys.executable, "-c", "print('nope')"],
            input="hello",
            stdin=subprocess.PIPE,
        )


def test_write_requires_piped_stdin() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    with pytest.raises(RuntimeError, match="stdin is not available"):
        process.write("ignored")
    process.wait()


def test_exec_script_runs_uv_shebang_with_lf() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "uv_script.py"
        script_path.write_text(
            "#!/usr/bin/env -S uv run --script\n"
            "print('uv shebang works')\n",
            encoding="utf-8",
        )

        result = RunningProcess.exec_script(script_path)
        assert result.stdout is not None
        assert result.stdout.strip() == "uv shebang works"


def test_exec_script_runs_uv_shebang_with_crlf() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "uv_script_crlf.py"
        script_path.write_bytes(
            b"#!/usr/bin/env -S uv run --script\r\n"
            b"print('uv shebang crlf works')\r\n"
        )

        result = RunningProcess.exec_script(script_path)
        assert result.stdout is not None
        assert result.stdout.strip() == "uv shebang crlf works"


def test_exec_script_without_shebang_raises() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "plain.py"
        script_path.write_text("print('no shebang')\n", encoding="utf-8")

        with pytest.raises(ValueError, match="does not start with a shebang"):
            RunningProcess.exec_script(script_path)


def test_run_filter_input_uses_native_path_for_text() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.read(); "
                "print(data.upper()); "
                "print('stderr:' + data.lower(), file=sys.stderr)"
            ),
        ],
        input="AbC\n",
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "ABC" in lines
    assert "stderr:abc" in lines
    assert result.stderr is None


def test_run_filter_input_uses_native_path_for_bytes() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.buffer.read(); "
                "sys.stdout.buffer.write(data[::-1]); "
                "sys.stderr.buffer.write(data.upper())"
            ),
        ],
        input=b"abc",
        capture_output=True,
        text=False,
        timeout=5,
    )
    assert result.stdout is not None
    assert b"cba" in result.stdout
    assert b"ABC" in result.stdout
    assert result.stderr is None


def test_run_supports_detached_stdin_with_devnull() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.read(); "
                "print('stdin_closed' if data == '' else 'stdin_open'); "
                "print('err-ok', file=sys.stderr)"
            ),
        ],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "stdin_closed" in lines
    assert "err-ok" in lines
    assert result.stderr is None


def test_run_supports_shell_and_env() -> None:
    result = RunningProcess.run(
        "python -c \"import os; print(os.environ['RP_TEST_VALUE'])\"",
        shell=True,
        env={**os.environ, "RP_TEST_VALUE": "shell-ok"},
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout == "shell-ok"


def test_subprocess_run_capture_output() -> None:
    result = subprocess_run(
        command=[sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        cwd=None,
        check=False,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "out" in lines
    assert "err" in lines
    assert result.stderr is None


def test_subprocess_run_timeout_raises_runtime_error() -> None:
    with pytest.raises(RuntimeError, match="Process timed out"):
        subprocess_run(
            command=[sys.executable, "-c", "import time; time.sleep(5)"],
            cwd=None,
            check=False,
            timeout=0.1,  # type: ignore[arg-type]
        )


def test_run_streaming_echoes_both_streams(capsys: pytest.CaptureFixture[str]) -> None:
    code = RunningProcess.run_streaming(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        timeout=5,
    )
    captured = capsys.readouterr()
    assert code == 0
    assert "out" in captured.out
    assert "err" in captured.err


def test_capture_false_does_not_store_output() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hidden')"], capture=False)
    process.wait()
    assert process.stdout == ""
    assert process.stderr == ""


def test_invalid_string_command_without_shell() -> None:
    with pytest.raises(ValueError, match="String commands require shell=True"):
        RunningProcess("echo nope", shell=False)


def test_invalid_echo_type_raises() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    with pytest.raises(TypeError):
        process.wait(echo="bad")  # type: ignore[arg-type]


def test_echo_true_writes_stdout_only(capsys: pytest.CaptureFixture[str]) -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    process.wait(echo=True)
    captured = capsys.readouterr()
    assert "hello" in captured.out


def test_wait_uses_instance_timeout_and_callback() -> None:
    seen: list[ProcessInfo] = []
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        timeout=0.1,
        on_timeout=seen.append,
    )
    with pytest.raises(TimeoutError):
        process.wait()
    assert len(seen) == 1
    assert seen[0].pid != 0
    assert seen[0].command == [sys.executable, "-c", "import time; time.sleep(10)"]


def test_wait_raises_keyboard_interrupt_when_child_gets_sigint() -> None:
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if sys.platform == "win32" else None
    )
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(30)\n"
                "except KeyboardInterrupt:\n"
                "    print('child-interrupted', flush=True)\n"
                "    raise\n"
            ),
        ],
        creationflags=creationflags,
        timeout=2,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait()


def test_exit_status_classifies_possible_oom_for_sigkill_on_unix() -> None:
    status = classify_exit_status(-9, set(), platform="linux")
    assert status.signal_number == 9
    assert status.possible_oom is True
    assert status.abnormal is True


def test_exit_status_classifies_windows_no_memory_status() -> None:
    status = classify_exit_status(-1073741801, set(), platform="win32")
    assert status.possible_oom is True
    assert status.abnormal is True


def test_wait_echo_includes_stderr(capsys: pytest.CaptureFixture[str]) -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
    )
    process.wait(echo=True)
    captured = capsys.readouterr()
    assert "out" in captured.out
    assert "err" in captured.err


def test_echo_true_is_safe_for_ascii_console(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="ascii", errors="strict")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess([sys.executable, "-c", "print('snowman: \\u2603')"])
    process.wait(echo=True)
    fake_stdout.flush()
    assert b"snowman: ?" in fake_stdout.buffer.getvalue()


def test_process_utils_handle_invalid_pid() -> None:
    assert "Could not get process info" in get_process_tree_info(999999)
    kill_process_tree(999999)


def test_process_utils_describe_current_process() -> None:
    info = get_process_tree_info(os.getpid())
    assert f"Process {os.getpid()}" in info
    assert "Status:" in info


def test_child_python_env_defaults_to_utf8_replace() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import os, sys; "
                "print(os.environ.get('PYTHONUTF8', '')); "
                "print(os.environ.get('PYTHONUNBUFFERED', '')); "
                "print(sys.stdout.encoding)"
            ),
        ]
    )
    process.wait()
    assert process.stdout.splitlines() == ["1", "1", "utf-8"]


def test_running_process_can_set_positive_nice() -> None:
    if sys.platform == "win32":
        script = (
            "import psutil; "
            "print(psutil.Process().nice())"
        )
        process = RunningProcess([sys.executable, "-c", script], nice=5)
        process.wait()
        assert int(process.stdout) == psutil.BELOW_NORMAL_PRIORITY_CLASS
        return

    process = RunningProcess(
        [sys.executable, "-c", "import os; print(os.nice(0))"],
        nice=5,
    )
    process.wait()
    assert int(process.stdout) >= 5


def test_running_process_accepts_platform_neutral_priority_enum() -> None:
    if sys.platform == "win32":
        script = "import psutil; print(psutil.Process().nice())"
        process = RunningProcess([sys.executable, "-c", script], nice=CpuPriority.LOW)
        process.wait()
        assert int(process.stdout) == psutil.BELOW_NORMAL_PRIORITY_CLASS
        return

    process = RunningProcess(
        [sys.executable, "-c", "import os; print(os.nice(0))"],
        nice=CpuPriority.LOW,
    )
    process.wait()
    assert int(process.stdout) >= 5


def test_running_process_rejects_invalid_nice_type() -> None:
    with pytest.raises(TypeError, match="nice must be an int, CpuPriority, or None"):
        RunningProcess([sys.executable, "-c", "print('x')"], auto_run=False, nice="low")  # type: ignore[arg-type]


def test_process_utils_reports_child_processes() -> None:
    script = """
import subprocess
import sys
import time

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"])
print(child.pid, flush=True)
time.sleep(10)
"""
    parent = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert parent.stdout is not None
        child_pid_line = parent.stdout.readline().strip()
        assert child_pid_line
        child_pid = int(child_pid_line)

        info = get_process_tree_info(parent.pid)
        assert f"Process {parent.pid}" in info
        assert f"Child {child_pid}" in info
    finally:
        kill_process_tree(parent.pid)
        with contextlib.suppress(Exception):
            parent.wait(timeout=2)


def test_kill_process_tree_kills_parent_and_child() -> None:
    script = """
import subprocess
import sys
import time

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"])
print(child.pid, flush=True)
time.sleep(10)
"""
    parent = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert parent.stdout is not None
    child_pid_line = parent.stdout.readline().strip()
    assert child_pid_line
    child_pid = int(child_pid_line)

    kill_process_tree(parent.pid)

    parent.wait(timeout=3)
    assert not psutil.pid_exists(parent.pid)
    assert not psutil.pid_exists(child_pid)


def test_running_process_force_killed_parent_reaps_child_and_cleans_registry() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific parent crash behavior")

    script = """
import sys
import time
from running_process import RunningProcess

process = RunningProcess([sys.executable, "-c", "import time; time.sleep(30)"])
print(process.pid, flush=True)
time.sleep(30)
"""
    owner = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert owner.stdout is not None
        child_pid = int(owner.stdout.readline().strip())

        deadline = time.time() + 5.0
        while time.time() < deadline:
            if any(
                entry.pid == child_pid and entry.kind == "running_process"
                for entry in list_tracked_processes()
            ):
                break
            time.sleep(0.05)
        else:
            raise AssertionError(f"running process pid {child_pid} was not registered")

        owner.kill()
        owner.wait(timeout=5)

        deadline = time.time() + 5.0
        while time.time() < deadline and psutil.pid_exists(child_pid):
            time.sleep(0.05)

        assert not psutil.pid_exists(child_pid)

        cleanup_tracked_processes()
        assert all(entry.pid != child_pid for entry in list_tracked_processes())
    finally:
        with contextlib.suppress(Exception):
            owner.kill()
        with contextlib.suppress(Exception):
            owner.wait(timeout=1)


def test_manager_dump_active_reports_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(1)"])
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        RunningProcessManagerSingleton.dump_active()
    process.kill()
    assert any("STUCK SUBPROCESS COMMANDS" in str(item.message) for item in caught)


def test_manager_dump_active_reports_empty_state() -> None:
    for process in RunningProcessManagerSingleton.list_active():
        process.kill()
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        RunningProcessManagerSingleton.dump_active()
    assert any("NO ACTIVE SUBPROCESSES DETECTED" in str(item.message) for item in caught)


def test_top_level_imports_preserve_output_formatter_exports() -> None:
    import running_process

    assert hasattr(running_process, "OutputFormatter")
    assert hasattr(running_process, "TimeDeltaFormatter")
