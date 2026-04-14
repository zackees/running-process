from __future__ import annotations

import contextlib
import gc
import io
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import warnings
from pathlib import Path

import pytest

from running_process import (
    DEVNULL,
    EOS,
    PIPE,
    CalledProcessError,
    CompletedProcess,
    CpuPriority,
    EndOfStream,
    ProcessAbnormalExit,
    ProcessInfo,
    ProcessOutputEvent,
    RunningProcess,
    RunningProcessManagerSingleton,
    TimeoutExpired,
    subprocess_run,
)
from running_process.exit_status import classify_exit_status
from running_process.process_utils import get_process_tree_info, kill_process_tree
from tests.process_helpers import (
    WINDOWS_BELOW_NORMAL_PRIORITY_CLASS,
    wait_for_pid_exit,
    windows_priority_class_script,
)

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)


@live
@skip_unless_github_actions
def test_finished_becomes_true_without_poll() -> None:
    """Regression test for issue #7: .finished never becomes True without explicit .poll()."""
    timeout = 10
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.finished:
            break
        time.sleep(0.05)
    assert process.finished, "Process.finished never became True without explicit poll()"
    assert process.returncode == 0


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


def test_default_capture_merges_stderr_into_stdout() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script])
    code = process.wait()

    assert code == 0
    stdout_lines = process.stdout.strip().splitlines()
    combined_lines = process.combined_output.strip().splitlines()
    assert "out" in stdout_lines
    assert "err" in stdout_lines
    assert process.stderr == ""
    assert "out" in combined_lines
    assert "err" in combined_lines


def test_split_stdout_and_stderr_capture() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
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
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
    process.wait()

    assert process.stdout == "bad:\ufffd"
    assert process.stderr == "err:\ufffd"


def test_crlf_is_normalized_but_bare_cr_is_preserved() -> None:
    script = (
        "import sys; "
        "sys.stdout.buffer.write(b'first\\r\\nsecond\\rthird\\n'); "
        "sys.stdout.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
    process.wait()

    assert process.stdout == "first\nsecond\rthird"


def test_get_next_line_preserves_combined_stream() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

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
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

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
        ],
        stderr=PIPE,
    )

    stdout_line = process.get_next_stdout_line(timeout=2)
    assert stdout_line == "stdout-ready"
    assert process.stdout_stream.available() is False

    stderr_line = process.get_next_stderr_line(timeout=2)
    assert stderr_line == "stderr-ready"
    assert process.stderr_stream.available() is False
    assert process.wait() == 0


def test_running_process_iteration_yields_stdout_stderr_and_terminal_tuple() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        stderr=PIPE,
    )

    seen: list[tuple[object, object, int | None]] = []
    for stdout, stderr, exit_code in process:
        seen.append((stdout, stderr, exit_code))
        finished_and_drained = (
            (stdout is None or stdout is EOS)
            and (stderr is None or stderr is EOS)
            and exit_code is not None
        )
        if finished_and_drained:
            break

    assert any(stdout == "out" for stdout, _stderr, _code in seen)
    assert any(stderr == "err" for _stdout, stderr, _code in seen)
    assert seen[-1] == (EOS, EOS, 0)


def test_stream_iter_merges_stderr_into_stdout_by_default() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
    )

    events = list(process.stream_iter(timeout=5))

    assert any(event.stdout == "out" for event in events)
    assert any(event.stdout == "err" for event in events)
    assert all(event.stderr in (None, EOS) for event in events)
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 0)


def test_stream_iter_latches_exit_code_while_stderr_keeps_draining() -> None:
    child_code = (
        "import sys, time; "
        "time.sleep(0.2); "
        "print('late-stderr', file=sys.stderr, flush=True)"
    )
    script = (
        "import subprocess, sys; "
        f"subprocess.Popen([sys.executable, '-c', {child_code!r}]); "
        "sys.exit(1)"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

    events = list(process.stream_iter(timeout=5))

    assert any(
        event == ProcessOutputEvent(None, "late-stderr", 1) for event in events[:-1]
    )
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 1)
    assert events[-1].finished_and_drained is True


def test_stream_iter_reports_drained_streams_before_exit_code_when_needed() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.close(); "
                "sys.stderr.close(); "
                "time.sleep(0.2); "
                "sys.exit(7)"
            ),
        ],
        stderr=PIPE,
    )

    events = list(process.stream_iter(timeout=5))

    assert events[0].streams_drained is True
    if events[0].exit_code is None:
        assert events[0] == ProcessOutputEvent(EOS, EOS, None)
        assert events[0].finished_and_drained is False
    else:
        assert events[0] == ProcessOutputEvent(EOS, EOS, 7)
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 7)
    assert events[-1].finished_and_drained is True


def test_stream_iter_single_terminal_event_when_process_exits_without_output() -> None:
    process = RunningProcess([sys.executable, "-c", "import sys; sys.exit(4)"])

    events = list(process.stream_iter(timeout=5))

    assert events == [ProcessOutputEvent(EOS, EOS, 4)]
    assert events[0].finished_and_drained is True


def test_stream_iter_can_surface_exit_code_on_last_payload_before_terminal_event() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('done', file=sys.stderr, flush=True); sys.exit(3)",
        ],
        stderr=PIPE,
    )

    events = list(process.stream_iter(timeout=5))

    assert (
        ProcessOutputEvent(None, "done", None) in events
        or ProcessOutputEvent(None, "done", 3) in events
    )
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 3)


def test_stream_iter_times_out_when_no_output_and_process_has_not_exited() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(0.5)"],
    )

    iterator = process.stream_iter(timeout=0.05)
    with pytest.raises(TimeoutError, match="No stdout or stderr available before timeout"):
        next(iterator)

    process.wait(timeout=5)


def test_stream_iter_requires_capture_enabled() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        capture=False,
    )

    with pytest.raises(NotImplementedError, match="requires capture=True"):
        next(iter(process))

    process.wait(timeout=5)


def test_stream_iter_is_not_available_for_pty_backed_processes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('hello from pty')"],
        use_pty=True,
    )

    with pytest.raises(NotImplementedError, match="only available for pipe-backed"):
        next(iter(process))

    process.wait(timeout=5)


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
        [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
        stderr=PIPE,
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
        [sys.executable, "-c", "import sys; print('alpha'); print('beta', file=sys.stderr)"],
        stderr=PIPE,
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


def test_terminate_finishes_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    process.terminate()
    assert process.finished


def test_manager_unregisters_after_wait() -> None:
    before = len(RunningProcessManagerSingleton.list_active())
    RunningProcess.run([sys.executable, "-c", "print('manager')"], capture_output=True)
    after = len(RunningProcessManagerSingleton.list_active())
    assert before == after


def test_running_process_manager_register_normalizes_pathlike_cwd(monkeypatch) -> None:
    from running_process.running_process_manager import RunningProcessManager

    seen: dict[str, object] = {}

    def fake_register(pid: int, kind: str, command: str, cwd: str | None) -> None:
        seen["pid"] = pid
        seen["kind"] = kind
        seen["command"] = command
        seen["cwd"] = cwd

    monkeypatch.setattr(
        "running_process.running_process_manager.native_register_process",
        fake_register,
    )

    proc = type(
        "FakeProc",
        (),
        {
            "pid": 123,
            "command": ["python", "-m", "ci.test"],
            "cwd": Path("C:/tmp/example"),
            "use_pty": False,
        },
    )()

    RunningProcessManager().register(proc)

    assert seen == {
        "pid": 123,
        "kind": "subprocess",
        "command": "python -m ci.test",
        "cwd": str(Path("C:/tmp/example")),
    }


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
        assert wait_for_pid_exit(pid, 3.0, before_sleep=gc.collect)


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
    assert "err" in captured.out
    assert captured.err == ""


def test_run_streaming_stdout_callback_receives_output() -> None:
    lines: list[str] = []
    code = RunningProcess.run_streaming(
        [sys.executable, "-c", "print('hello'); print('world')"],
        timeout=5,
        stdout_callback=lines.append,
    )
    assert code == 0
    combined = "".join(lines)
    assert "hello" in combined
    assert "world" in combined


def test_run_streaming_stdout_callback_suppresses_console_echo(
    capsys: pytest.CaptureFixture[str],
) -> None:
    lines: list[str] = []
    RunningProcess.run_streaming(
        [sys.executable, "-c", "print('captured')"],
        timeout=5,
        stdout_callback=lines.append,
    )
    captured = capsys.readouterr()
    assert "captured" not in captured.out
    assert "captured" in "".join(lines)


def test_run_streaming_rejects_unknown_kwargs() -> None:
    with pytest.raises(TypeError):
        RunningProcess.run_streaming(
            [sys.executable, "-c", "print('x')"],
            bogus_kwarg="should fail",  # type: ignore[call-arg]
        )


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


def test_is_runninng_compat_alias() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(0.2)"])
    assert process.is_runninng() is True
    process.wait()
    assert process.is_runninng() is False


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
                "    time.sleep(2)\n"
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


def test_wait_raises_keyboard_interrupt_promptly_while_main_thread_is_blocked() -> None:
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if sys.platform == "win32" else None
    )
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(2)\n"
                "except KeyboardInterrupt:\n"
                "    raise\n"
            ),
        ],
        creationflags=creationflags,
        timeout=5,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"

    sent_at: list[float] = []

    def trigger_interrupt() -> None:
        time.sleep(0.1)
        sent_at.append(time.perf_counter())
        process.send_interrupt()

    worker = threading.Thread(target=trigger_interrupt, daemon=True)
    worker.start()

    with pytest.raises(KeyboardInterrupt):
        process.wait()

    worker.join(timeout=1)
    assert sent_at
    assert time.perf_counter() - sent_at[0] < 0.2


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
    assert "err" in captured.out
    assert captured.err == ""


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
        script = windows_priority_class_script()
        process = RunningProcess([sys.executable, "-c", script], nice=5)
        process.wait()
        assert int(process.stdout) == WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        return

    process = RunningProcess(
        [sys.executable, "-c", "import os; print(os.nice(0))"],
        nice=5,
    )
    process.wait()
    assert int(process.stdout) >= 5


def test_running_process_accepts_platform_neutral_priority_enum() -> None:
    if sys.platform == "win32":
        script = windows_priority_class_script()
        process = RunningProcess([sys.executable, "-c", script], nice=CpuPriority.LOW)
        process.wait()
        assert int(process.stdout) == WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
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
    assert wait_for_pid_exit(parent.pid, 3.0)
    assert wait_for_pid_exit(child_pid, 3.0)


def test_running_process_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific parent crash behavior")

    script = """
import sys
import time
from running_process import RunningProcess

process = RunningProcess([sys.executable, "-c", "import time; time.sleep(2)"])
print(process.pid, flush=True)
time.sleep(2)
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

        owner.kill()
        owner.wait(timeout=5)

        assert wait_for_pid_exit(child_pid, 5.0)
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


def test_allows_child_ctrl_c_false_child_does_not_see_sigint() -> None:
    """When allows_child_ctrl_c_interruption=False, send_interrupt kills the child
    but the child's own SIGINT handler never fires."""
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import signal, time, sys\n"
                "got_sigint = False\n"
                "def handler(sig, frame):\n"
                "    global got_sigint\n"
                "    got_sigint = True\n"
                "    print('child-saw-sigint', flush=True)\n"
                "signal.signal(signal.SIGINT, handler)\n"
                "print('ready', flush=True)\n"
                "time.sleep(2)\n"
            ),
        ],
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    # The parent kills the child — child never sees SIGINT
    process.kill()
    # Ensure child did not print "child-saw-sigint"
    remaining = process.stdout
    assert "child-saw-sigint" not in remaining


def test_allows_child_ctrl_c_false_process_group_isolation() -> None:
    """When allows_child_ctrl_c_interruption=False, the child is in its own
    process group and send_interrupt still works to explicitly interrupt it."""
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(2)\n"
                "except KeyboardInterrupt:\n"
                "    raise\n"
            ),
        ],
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    # send_interrupt still works (explicit API call)
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait()


def test_allows_child_ctrl_c_false_wait_kills_on_keyboard_interrupt() -> None:
    """When allows_child_ctrl_c_interruption=False, KeyboardInterrupt during
    wait() kills the child before re-raising."""
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import time; print('ready', flush=True); time.sleep(2)",
        ],
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"

    def trigger_interrupt() -> None:
        time.sleep(0.2)
        process.send_interrupt()

    worker = threading.Thread(target=trigger_interrupt, daemon=True)
    worker.start()

    with pytest.raises(KeyboardInterrupt):
        process.wait()
    worker.join(timeout=5)
    # Process should be dead — killed by the wait() handler
    assert process.returncode is not None
