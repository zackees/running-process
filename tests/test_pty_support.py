from __future__ import annotations

import contextlib
import faulthandler
import gc
import io
import subprocess
import sys
import tempfile
import threading
import time
import warnings
from pathlib import Path
from types import SimpleNamespace

import pytest
from running_process._native import native_windows_terminal_input_bytes

import running_process.pty as pty_module
import running_process.running_process as running_process_module
from running_process import (
    CpuPriority,
    Expect,
    ExpectRule,
    Idle,
    IdleDecision,
    IdleDetection,
    IdleStartTrigger,
    IdleTiming,
    InteractiveMode,
    ProcessIdleDetection,
    PtyIdleDetection,
    RunningProcess,
    WaitCallbackResult,
)
from running_process.pty import (
    IdleContext,
    IdleDiff,
    IdleInfoDiff,
    IdleWaitResult,
    InteractiveProcess,
    InterruptResult,
    PseudoTerminalProcess,
    Pty,
    WaitForResult,
    interactive_launch_spec,
)
from tests.process_helpers import (
    WINDOWS_BELOW_NORMAL_PRIORITY_CLASS,
    pid_exists,
    wait_for_pid_exit,
    windows_priority_class_script,
)

_PTY_SUPPORT_WATCHDOG_TIMEOUT_SECONDS = 120.0


@pytest.fixture(autouse=True)
def _suppress_pty_text_warning_by_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")


@pytest.fixture(scope="module", autouse=True)
def _pty_support_module_watchdog() -> None:
    faulthandler.dump_traceback_later(
        _PTY_SUPPORT_WATCHDOG_TIMEOUT_SECONDS,
        file=sys.__stderr__,
        exit=True,
    )
    try:
        yield
    finally:
        faulthandler.cancel_dump_traceback_later()


def _read_until_contains(process: object, needle: str, timeout: float = 10) -> str:
    deadline = time.time() + timeout
    chunks: list[str] = []
    while time.time() < deadline:
        try:
            chunk = process.read(timeout=0.2)  # type: ignore[attr-defined]
        except TimeoutError:
            continue
        except EOFError:
            break
        text = chunk.decode("utf-8", errors="replace") if isinstance(chunk, bytes) else chunk
        chunks.append(text)
        if needle in "".join(chunks):
            return "".join(chunks)
    raise AssertionError(f"Did not observe {needle!r} in PTY output: {''.join(chunks)!r}")


def _capture_wait_echo_bytes(process: RunningProcess) -> bytes:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    original_stdout = sys.stdout
    sys.stdout = fake_stdout
    try:
        assert process.wait(timeout=5, echo=True) == 0
        fake_stdout.flush()
        return fake_stdout.buffer.getvalue()
    finally:
        sys.stdout = original_stdout


def test_pseudo_terminal_round_trips_interactive_io() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.write('ready>'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "sys.stdout.write(f'echo:{line}\\n'); sys.stdout.flush()"
            ),
        ],
        text=True,
    )

    initial = process.expect("ready>", timeout=5, action="hello from pty\n")
    assert initial.matched == "ready>"
    echoed = process.expect("echo:hello from pty", timeout=5)
    assert echoed.matched == "echo:hello from pty"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_accepts_string_command_and_auto_splits() -> None:
    process = RunningProcess.pseudo_terminal(
        f"{sys.executable} -c \"print('string command ok')\"",
        text=True,
    )
    output = _read_until_contains(process, "string command ok")
    assert "string command ok" in output
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_preserves_carriage_returns() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            ("import sys, time; sys.stdout.write('first\\rsecond\\n'); sys.stdout.flush()"),
        ],
        text=True,
    )
    output = _read_until_contains(process, "second")
    assert "\r" in output
    process.wait(timeout=5)


def test_pseudo_terminal_wait_echo_preserves_ansi_color_sequences(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            ("import sys; sys.stdout.buffer.write(b'\\x1b[31mred\\x1b[0m'); sys.stdout.flush()"),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert echoed == process.stdout
    assert b"red" in echoed
    assert b"\x1b[" in echoed


def test_pseudo_terminal_wait_echo_enables_windows_vt_output_for_ansi_sequences(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(pty_module, "_WINDOWS_VT_OUTPUT_HANDLES", set())

    handles: list[int] = []

    def fake_windows_console_output_handle(stream: io.TextIOWrapper) -> int:
        return 77

    def fake_enable_windows_vt_output_handle(handle: int) -> bool:
        handles.append(handle)
        return True

    monkeypatch.setattr(
        pty_module,
        "_windows_console_output_handle",
        fake_windows_console_output_handle,
    )
    monkeypatch.setattr(
        pty_module,
        "_enable_windows_vt_output_handle",
        fake_enable_windows_vt_output_handle,
    )

    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.buffer.write(b'\\x1b[?25l\\x1b[31mred\\x1b[0m\\x1b[?25h'); "
                "sys.stdout.flush()"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert handles == [77]
    assert echoed == process.stdout
    assert b"red" in echoed
    assert b"\x1b[" in echoed


def test_safe_console_write_chunk_enables_windows_vt_output_once(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(pty_module, "_WINDOWS_VT_OUTPUT_HANDLES", set())

    handles: list[int] = []

    def fake_windows_console_output_handle(stream: io.TextIOWrapper) -> int:
        return 99

    def fake_enable_windows_vt_output_handle(handle: int) -> bool:
        handles.append(handle)
        return True

    monkeypatch.setattr(
        pty_module,
        "_windows_console_output_handle",
        fake_windows_console_output_handle,
    )
    monkeypatch.setattr(
        pty_module,
        "_enable_windows_vt_output_handle",
        fake_enable_windows_vt_output_handle,
    )

    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")

    pty_module._safe_console_write_chunk(
        fake_stdout,
        b"\x1b[?25l",
        encoding="utf-8",
        errors="replace",
    )
    pty_module._safe_console_write_chunk(
        fake_stdout,
        b"\x1b[?25h",
        encoding="utf-8",
        errors="replace",
    )

    fake_stdout.flush()
    assert handles == [99]
    assert fake_stdout.buffer.getvalue() == b"\x1b[?25l\x1b[?25h"


def test_running_process_use_pty_text_mode_warns_and_falls_back_to_bytes() -> None:
    with pytest.MonkeyPatch.context() as monkeypatch:
        monkeypatch.delenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", raising=False)
        with pytest.warns(
            RuntimeWarning,
            match="PTY mode ignores text/universal_newlines and always uses raw bytes",
        ):
            process = RunningProcess(
                [sys.executable, "-c", "print('warn')"],
                use_pty=True,
                text=True,
            )
    assert process.wait(timeout=5) == 0
    assert isinstance(process.stdout, bytes)


def test_running_process_use_pty_text_mode_warning_can_be_suppressed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        process = RunningProcess(
            [sys.executable, "-c", "print('quiet')"],
            use_pty=True,
            text=True,
        )
        assert process.wait(timeout=5) == 0
    runtime_warnings = [item for item in caught if issubclass(item.category, RuntimeWarning)]
    assert runtime_warnings == []


def test_pseudo_terminal_text_mode_warns_and_output_remains_bytes() -> None:
    with pytest.MonkeyPatch.context() as monkeypatch:
        monkeypatch.delenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", raising=False)
        with pytest.warns(
            RuntimeWarning,
            match="PTY mode ignores text/universal_newlines and always uses raw bytes",
        ):
            process = RunningProcess.pseudo_terminal(
                [sys.executable, "-c", "print('warn')"],
                text=True,
            )
    assert process.wait(timeout=5) == 0
    assert isinstance(process.output, bytes)


def test_pseudo_terminal_wait_echo_preserves_carriage_return_redraws(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.write('first'); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.write('\\rsecond'); sys.stdout.flush()"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert echoed == process.stdout
    assert b"first" in echoed
    assert b"second" in echoed
    assert any(marker in echoed for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_wait_echo_preserves_progress_redraw_sequences() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "for value in (10, 20, 30):\n"
                "    sys.stdout.write(f'progress {value}%\\r')\n"
                "    sys.stdout.flush()\n"
                "    time.sleep(0.02)\n"
                "sys.stdout.write('done\\n')\n"
                "sys.stdout.flush()\n"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    echoed = _capture_wait_echo_bytes(process)

    assert echoed == process.stdout
    assert b"progress 10%" in echoed
    assert b"progress 20%" in echoed
    assert b"progress 30%" in echoed
    assert b"done\r\n" in echoed
    assert any(marker in echoed for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_progress_capture_preserves_redraw_markers() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "for value in (10, 20, 30):\n"
                "    sys.stdout.write(f'progress {value}%\\r')\n"
                "    sys.stdout.flush()\n"
                "    time.sleep(0.02)\n"
                "sys.stdout.write('done\\n')\n"
                "sys.stdout.flush()\n"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5) == 0
    assert isinstance(process.stdout, bytes)
    assert b"progress 10%" in process.stdout
    assert b"progress 20%" in process.stdout
    assert b"progress 30%" in process.stdout
    assert b"done\r\n" in process.stdout
    assert any(marker in process.stdout for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_internal_chunk_capture_keeps_cursor_home_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"first")
    process._handle_native_chunk(b"\x1b[25l\x1b[Hsecond\x1b[?25h")

    assert process.output == b"first\x1b[25l\x1b[Hsecond\x1b[?25h"
    assert process.drain_echo() == [b"first", b"\x1b[25l\x1b[Hsecond\x1b[?25h"]


def test_running_process_use_pty_defaults_to_no_capture() -> None:
    process = RunningProcess([sys.executable, "-c", "print('uncaptured')"], use_pty=True)

    assert process.wait(timeout=5) == 0
    assert process.stdout == b""
    assert process.stderr == b""
    assert process.combined_output == b""
    assert process.captured_output_bytes("stdout") == 0
    assert process.captured_output_bytes("combined") == 0


def test_running_process_use_pty_capture_true_retains_raw_bytes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('captured')"],
        use_pty=True,
        capture=True,
    )

    assert process.wait(timeout=5) == 0
    assert b"captured" in process.stdout


def test_running_process_use_pty_no_capture_still_supports_idle_detection() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            ("import sys, time\nsys.stdout.write('tick')\nsys.stdout.flush()\ntime.sleep(0.2)\n"),
        ],
        use_pty=True,
    )

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )

    assert result.idle_detected is True
    assert process.stdout == b""
    assert process.captured_output_bytes("combined") == 0


def test_pseudo_terminal_internal_chunk_capture_keeps_sgr_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[31mred\x1b[0m")

    assert process.output == b"\x1b[31mred\x1b[0m"
    assert process.drain_echo() == [b"\x1b[31mred\x1b[0m"]


def test_pseudo_terminal_internal_chunk_capture_keeps_query_and_title_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[6n\x1b]0;python\x07visible")

    assert process.output == b"\x1b[6n\x1b]0;python\x07visible"
    assert process.drain_echo() == [b"\x1b[6n\x1b]0;python\x07visible"]


def test_pseudo_terminal_internal_chunk_capture_keeps_cursor_motion_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[2Aup\x1b[1Bdown\x1b[2K")

    assert process.output == b"\x1b[2Aup\x1b[1Bdown\x1b[2K"
    assert process.drain_echo() == [b"\x1b[2Aup\x1b[1Bdown\x1b[2K"]


def test_pseudo_terminal_keeps_control_sequences_from_subprocess_output_untouched() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.buffer.write(b'prefix'); sys.stdout.flush(); "
                "payload = ("
                "    b'\\x1b[0;0;27;1;0;1_'"
                "    b'\\x1b[0;0;91;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;50;1;0;1_'"
                "    b'\\x1b[0;0;56;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;48;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;50;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "); "
                "sys.stdout.buffer.write(payload[:24]); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.buffer.write(payload[24:]); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.buffer.write(b'\\x1b[?2004l\\x1b[?1049lvisible\\r\\n'); "
                "sys.stdout.flush()"
            ),
        ],
        text=True,
    )

    output = _read_until_contains(process, "visible")
    assert "prefix" in output
    assert "visible" in output
    assert process.wait(timeout=5) == 0
    assert isinstance(process.output, bytes)
    assert b"\x1b" in process.output
    assert b"0;0;27;1;0;1_" in process.output
    assert b"?2004l" in process.output


def test_pseudo_terminal_discard_output_releases_history_bytes() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "print('alpha'); print('beta')"],
        text=True,
    )

    _read_until_contains(process, "beta")
    assert process.wait(timeout=5) == 0

    assert process.output_bytes >= len("alpha\nbeta")
    released = process.discard_output()
    assert released >= len("alpha\nbeta")
    assert process.output_bytes == 0
    assert process.output == b""


def test_pseudo_terminal_expect_sequence_runs_during_creation() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.write('name?'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "print('hello ' + line)"
            ),
        ],
        text=True,
        expect=[ExpectRule("name?", "pty user\n")],
        expect_timeout=5,
    )
    output = _read_until_contains(process, "hello pty user")
    assert "hello pty user" in output
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_expect_reports_pattern_not_found_on_eof() -> None:
    process = RunningProcess.pseudo_terminal([sys.executable, "-c", "print('done')"], text=True)
    with pytest.raises(EOFError, match="Pattern not found before stream closed"):
        process.expect("missing", timeout=5)


def test_interactive_launch_specs_model_windows_modes() -> None:
    pty_spec = interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
    shared_spec = interactive_launch_spec(InteractiveMode.CONSOLE_SHARED)
    isolated_spec = interactive_launch_spec(InteractiveMode.CONSOLE_ISOLATED)

    assert pty_spec.uses_pty is True
    assert pty_spec.ctrl_c_owner == "child"
    assert pty_spec.restore_terminal is True

    assert shared_spec.uses_pty is False
    assert shared_spec.ctrl_c_owner == "shared"
    assert shared_spec.restore_terminal is False

    assert isolated_spec.uses_pty is False
    assert isolated_spec.ctrl_c_owner == "parent"
    assert isolated_spec.restore_terminal is True


def test_pty_is_available_on_supported_platforms() -> None:
    if sys.platform in {"win32", "linux", "darwin"}:
        assert Pty.is_available() is True
    else:
        assert Pty.is_available() is False


def test_pseudo_terminal_kill_and_terminate_are_idempotent_after_exit() -> None:
    process = RunningProcess.pseudo_terminal([sys.executable, "-c", "print('done')"], text=True)
    assert process.wait(timeout=5) == 0
    process.kill()
    process.terminate()


def test_pseudo_terminal_kill_reaps_child_process() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        text=True,
    )
    pid = process.pid
    assert pid is not None

    process.kill()

    assert wait_for_pid_exit(pid, 3.0)
    assert process.poll() is not None
    assert not pid_exists(pid)


def test_pseudo_terminal_gc_reaps_child_process() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        text=True,
    )
    pid = process.pid
    assert pid is not None

    del process
    gc.collect()

    assert wait_for_pid_exit(pid, 3.0, before_sleep=gc.collect)


def test_pseudo_terminal_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific PTY parent crash behavior")

    script = (
        "import sys, time\n"
        "from running_process import RunningProcess\n"
        "process = RunningProcess.pseudo_terminal(\n"
        "    [\n"
        "        sys.executable,\n"
        "        '-c',\n"
        "        \"import sys, time; sys.stdout.write('username:'); sys.stdout.flush(); "
        'sys.stdin.readline(); time.sleep(2)",\n'
        "    ],\n"
        "    text=True,\n"
        ")\n"
        "print(process.pid, flush=True)\n"
        "time.sleep(2)\n"
    )

    owner = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert owner.stdout is not None
        child_line = owner.stdout.readline().strip()
        assert child_line.isdigit(), f"expected PTY child pid, got: {child_line!r}"
        child_pid = int(child_line)

        owner.kill()
        owner.wait(timeout=5)

        assert wait_for_pid_exit(child_pid, 5.0)
    finally:
        with contextlib.suppress(Exception):
            owner.kill()
        with contextlib.suppress(Exception):
            owner.wait(timeout=1)


def test_interactive_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific parent crash behavior")

    script = (
        "import sys, time\n"
        "from running_process import InteractiveMode, RunningProcess\n"
        "process = RunningProcess.interactive(\n"
        "    [sys.executable, '-c', \"import time; time.sleep(2)\"],\n"
        "    mode=InteractiveMode.CONSOLE_ISOLATED,\n"
        ")\n"
        "print(process.pid, flush=True)\n"
        "time.sleep(2)\n"
    )

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


def test_pseudo_terminal_interrupt_and_wait_reports_second_interrupt_success(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sent: list[str] = []
    waits = iter([False, True])

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = SimpleNamespace(send_interrupt=lambda: sent.append("interrupt"))
    monkeypatch.setattr(process, "_wait_until_exit", lambda timeout: next(waits))
    monkeypatch.setattr(process, "poll", lambda: 130 if len(sent) >= 2 else None)
    monkeypatch.setattr(process, "_drain_native_until_eof", lambda timeout: None)
    monkeypatch.setattr(process, "_finalize", lambda reason: None)

    result = process.interrupt_and_wait(grace_timeout=0.2, second_interrupt=True)

    assert isinstance(result, InterruptResult)
    assert sent == ["interrupt", "interrupt"]
    assert result.interrupt_count >= 2
    assert result.returncode == 130
    assert result.exit_reason == "interrupt"
    assert process.exit_reason == "interrupt"


def test_wait_with_idle_detector_none_preserves_int_return_type() -> None:
    process = RunningProcess([sys.executable, "-c", "print('done')"], use_pty=True, text=True)
    result = process.wait(timeout=5, idle_detector=None)
    assert isinstance(result, int)
    assert result == 0


def test_pseudo_terminal_wait_for_idle_uses_dataclass_config() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "print('tick', flush=True)\n"
                "time.sleep(0.05)\n"
                "print('tick', flush=True)\n"
                "time.sleep(1.0)\n"
            ),
        ],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.1,
                stability_window_seconds=0.05,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )
    assert isinstance(result, IdleWaitResult)
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert result.idle_for_seconds >= 0.1
    process.kill()


def test_pseudo_terminal_wait_for_idle_uses_callable_predicate() -> None:
    seen: list[IdleInfoDiff] = []

    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            ("import sys, time\nprint('noise', flush=True)\ntime.sleep(0.1)\n"),
        ],
        text=True,
    )

    def capture(diff: IdleInfoDiff) -> IdleDecision:
        seen.append(diff)
        if diff.pty_output_bytes > 0:
            return IdleDecision.ACTIVE
        if diff.process_alive:
            return IdleDecision.BEGIN_IDLE
        return IdleDecision.IS_IDLE

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            idle_reached=capture,
        ),
        timeout=1.0,
    )
    assert isinstance(result, IdleWaitResult)
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert seen
    assert all(item.delta_seconds >= 0.0 for item in seen)


def test_idle_reached_callback_accumulates_diff_when_callback_is_slow() -> None:
    seen: list[IdleInfoDiff] = []

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.18)"],
        text=True,
    )

    def capture(diff: IdleInfoDiff) -> IdleDecision:
        seen.append(diff)
        time.sleep(0.05)
        return IdleDecision.BEGIN_IDLE

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.04,
                stability_window_seconds=0.01,
                sample_interval_seconds=0.01,
            ),
            idle_reached=capture,
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert any(item.delta_seconds >= 0.03 for item in seen)
    process.kill()


def test_pseudo_terminal_wait_for_idle_hybrid_config_uses_custom_predicate() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.12)\n"
                "print('visible output', flush=True)\n"
                "time.sleep(0.5)\n"
            ),
        ],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.08,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            idle_reached=lambda diff: (
                IdleDecision.BEGIN_IDLE
                if diff.pty_output_bytes == 0 and diff.delta_seconds >= 0.02
                else IdleDecision.DEFAULT
            ),
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    process.kill()


def test_pseudo_terminal_wait_for_expect_on_callback_can_continue_until_exit() -> None:
    seen: list[str] = []
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('tick>'); sys.stdout.flush()\n"
                "first = sys.stdin.readline().strip()\n"
                "sys.stdout.write('tick>'); sys.stdout.flush()\n"
                "second = sys.stdin.readline().strip()\n"
                "sys.stdout.write(f'done:{first}:{second}\\n'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
    )

    def hook(match) -> WaitCallbackResult:
        seen.append(match.matched)
        if len(seen) == 1:
            return WaitCallbackResult.CONTINUE
        return WaitCallbackResult.EXIT

    result = process.wait_for(
        Expect("tick>", action="go\n", on_callback=hook),
        timeout=5.0,
    )

    assert isinstance(result, WaitForResult)
    assert result.matched is True
    assert result.condition is not None
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "tick>"
    assert seen == ["tick>", "tick>"]
    assert process.expect("done:go:go", timeout=5).matched == "done:go:go"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_expect_not_suppresses_trigger() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('ERROR>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "sys.stdout.write('DONE>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    writer = threading.Thread(
        target=lambda: (
            time.sleep(0.05),
            process.write("\n"),
            time.sleep(0.05),
            process.write("\n"),
        ),
        daemon=True,
    )
    writer.start()
    result = process.wait_for(
        Expect("DONE>", NOT="ERROR>"),
        timeout=5.0,
    )
    writer.join(timeout=1.0)

    assert result.matched is False
    assert result.exit_reason == "process_exit"
    assert result.returncode == 0


def test_pseudo_terminal_wait_for_idle_on_callback_can_disarm_and_allow_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.12)\n"
                "sys.stdout.write('DONE>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    def disarm_idle(_result: IdleWaitResult) -> WaitCallbackResult:
        return WaitCallbackResult.CONTINUE_AND_DISARM

    result = process.wait_for(
        Idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                )
            ),
            on_callback=disarm_idle,
        ),
        Expect("DONE>", action="\n"),
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "DONE>"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_on_callback_buffer_can_answer_prompts() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "username = sys.stdin.readline().strip()\n"
                "sys.stdout.write('password:'); sys.stdout.flush()\n"
                "password = sys.stdin.readline().strip()\n"
                "sys.stdout.write(f'ok:{username}:{password}\\n'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
    )

    def send_username(_match, buffer) -> WaitCallbackResult:
        buffer.write("alice\n")
        return WaitCallbackResult.CONTINUE_AND_DISARM

    def send_password(_match, buffer) -> WaitCallbackResult:
        buffer.write("secret\n")
        return WaitCallbackResult.CONTINUE_AND_DISARM

    result = process.wait_for(
        Expect("username:", on_callback=send_username),
        Expect("password:", on_callback=send_password),
        Expect("ok:alice:secret"),
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "ok:alice:secret"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_on_callback_propagates_keyboard_interrupt() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    def interrupt(_match, _buffer) -> WaitCallbackResult:
        raise KeyboardInterrupt

    try:
        with pytest.raises(KeyboardInterrupt):
            process.wait_for(
                Expect("username:", on_callback=interrupt),
                timeout=5.0,
            )
    finally:
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_expect_can_chain_next_expect() -> None:
    def send_username(_match, buffer) -> WaitCallbackResult:
        buffer.write("alice\n")
        return WaitCallbackResult.EXIT

    def send_password(_match, buffer) -> WaitCallbackResult:
        buffer.write("secret\n")
        return WaitCallbackResult.EXIT

    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "username = sys.stdin.readline().strip()\n"
                "sys.stdout.write('password:'); sys.stdout.flush()\n"
                "password = sys.stdin.readline().strip()\n"
                "sys.stdout.write(f'ok:{username}:{password}\\n'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
        expect=[Expect("username:", on_callback=send_username)],
    )

    first = process.wait_for_expect(
        next_expect=Expect("password:", on_callback=send_password),
        timeout=5.0,
    )
    assert first.matched is True
    assert first.expect_match is not None
    assert first.expect_match.matched == "username:"

    second = process.wait_for_expect(
        next_expect=Expect("ok:alice:secret"),
        timeout=5.0,
    )
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "password:"

    third = process.wait_for_expect(timeout=5.0)
    assert third.matched is True
    assert third.expect_match is not None
    assert third.expect_match.matched == "ok:alice:secret"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_expect_timeout_preserves_registered_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.15)\n"
                "sys.stdout.write('ready>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
        expect=[Expect("ready>", action="\n")],
    )

    first = process.wait_for_expect(timeout=0.05)
    assert first.matched is False
    assert first.exit_reason == "timeout"

    second = process.wait_for_expect(timeout=5.0)
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "ready>"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_expect_timeout_does_not_arm_next_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.12)\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "sys.stdout.write('password:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
        expect=[Expect("username:", action="alice\n")],
    )

    first = process.wait_for_expect(
        next_expect=Expect("password:", action="secret\n"),
        timeout=0.05,
    )
    assert first.matched is False
    assert first.exit_reason == "timeout"

    second = process.wait_for_expect(timeout=5.0)
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "username:"

    third = process.wait_for_expect(
        next_expect=Expect("password:", action="secret\n"),
        timeout=5.0,
    )
    assert third.matched is True
    assert third.expect_match is not None
    assert third.expect_match.matched == "password:"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_constructor_can_mix_expect_rule_and_registered_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('bootstrap:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "sys.stdout.write('armed:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
        expect=[
            ExpectRule("bootstrap:", "boot\n"),
            Expect("armed:", action="armed\n"),
        ],
        expect_timeout=5.0,
    )

    result = process.wait_for_expect(timeout=5.0)
    assert result.matched is True
    assert result.expect_match is not None
    assert result.expect_match.matched == "armed:"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_callable_condition_does_not_block_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.02)\n"
                "sys.stdout.write('ready>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    def slow_false() -> bool:
        time.sleep(0.3)
        return False

    result = process.wait_for(
        Expect("ready>", action="\n"),
        slow_false,
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "ready>"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_idle_reports_process_exit_before_idle() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.05)"],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.4,
                stability_window_seconds=0.05,
                sample_interval_seconds=0.02,
            ),
            idle_reached=lambda _diff: IdleDecision.ACTIVE,
        ),
        timeout=1.0,
    )
    assert result.idle_detected is False
    assert result.exit_reason == "process_exit"
    assert result.returncode == 0


def test_pseudo_terminal_wait_for_idle_honors_stability_window() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            ("import sys, time\nprint('start', flush=True)\ntime.sleep(0.4)\n"),
        ],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.15,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert result.idle_for_seconds >= 0.15
    process.kill()


def test_pseudo_terminal_wait_for_idle_passes_diff_and_context_to_predicate() -> None:
    seen: list[tuple[IdleDiff, IdleContext]] = []

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.3)"],
        text=True,
    )

    def capture(diff: IdleDiff, ctx: IdleContext) -> bool:
        seen.append((diff, ctx))
        return False

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            predicate=capture,
        ),
        timeout=1.0,
    )
    assert result.exit_reason == "idle_timeout"
    assert seen
    assert all(item[0].process_alive is True for item in seen[:1])


def test_idle_detection_rejects_conflicting_custom_callback_fields() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.2)"],
        text=True,
    )
    with pytest.raises(ValueError, match="mutually exclusive"):
        process.wait_for_idle(
            IdleDetection(
                idle_reached=lambda _diff: IdleDecision.ACTIVE,
                predicate=lambda _diff, _ctx: False,
            ),
            timeout=0.1,
        )
    process.kill()


def test_idle_reached_callback_requires_idle_decision_result() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.2)"],
        text=True,
    )
    with pytest.raises(TypeError, match="IdleDecision"):
        process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=5.0,
                    stability_window_seconds=0.01,
                    sample_interval_seconds=0.01,
                ),
                idle_reached=lambda _diff: False,  # type: ignore[return-value]
            ),
            timeout=0.2,
        )
    process.kill()


def test_running_process_interactive_launches_console_mode() -> None:
    process = RunningProcess.interactive(
        [sys.executable, "-c", "print('interactive')"],
        mode=InteractiveMode.CONSOLE_SHARED,
    )
    assert process.wait(timeout=5) == 0


def test_running_process_signal_bool_shadows_python_reads() -> None:
    signal = RunningProcess.SignalBool(False)
    assert signal.value is False
    assert bool(signal) is False
    assert signal.load() is False

    signal.store(True)

    assert signal.value is True
    assert bool(signal) is True
    assert signal.load() is True
    assert signal.compare_and_swap(True, False) is True
    assert signal.value is False
    assert signal.compare_and_swap(True, True) is False
    assert signal.value is False


def test_pseudo_terminal_idle_timeout_signal_can_be_reenabled_during_wait() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(1.5)"],
        text=True,
    )
    process.idle_timeout_enabled = False

    def enable_later() -> None:
        time.sleep(0.3)
        process.idle_timeout_enabled = True

    worker = threading.Thread(target=enable_later, daemon=True)
    worker.start()
    started = time.time()
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.2,
                stability_window_seconds=0.1,
                sample_interval_seconds=0.05,
            )
        ),
        timeout=2.0,
    )
    elapsed = time.time() - started
    worker.join(timeout=2.0)

    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert elapsed >= 0.3
    process.kill()


def test_pseudo_terminal_wait_for_idle_does_not_arm_input_submit_on_newline_bytes() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n")

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
            ),
            timeout=0.8,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is False
        assert result.exit_reason == "process_exit"
        assert elapsed >= 0.25
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_can_arm_on_explicit_input_submit() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n", submit=True)

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
            ),
            timeout=0.35,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is True
        assert result.exit_reason == "idle_timeout"
        assert elapsed >= 0.15
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_can_arm_on_input_newline() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n")

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_NEWLINE),
            ),
            timeout=0.35,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is True
        assert result.exit_reason == "idle_timeout"
        assert elapsed >= 0.15
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_condition_can_arm_on_explicit_input_submit() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n", submit=True)

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for(
            Idle(
                IdleDetection(
                    timing=IdleTiming(
                        timeout_seconds=0.05,
                        stability_window_seconds=0.02,
                        sample_interval_seconds=0.01,
                    ),
                    pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
                )
            ),
            timeout=0.35,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.matched is True
        assert result.exit_reason == "condition_met"
        assert result.idle_result is not None
        assert result.idle_result.idle_detected is True
        assert result.idle_result.exit_reason == "idle_timeout"
        assert elapsed >= 0.15
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_running_process_use_pty_forwards_terminal_input_relay_options(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    recorded: dict[str, object] = {}

    class FakePtyProcess:
        def __init__(self, command: str | list[str], **kwargs: object) -> None:
            recorded["command"] = command
            recorded["kwargs"] = kwargs

        def start(self) -> None:
            return None

    monkeypatch.setattr(running_process_module, "PseudoTerminalProcess", FakePtyProcess)

    process = RunningProcess(
        [sys.executable, "-c", "print('relay')"],
        use_pty=True,
        auto_run=False,
        relay_terminal_input=True,
        arm_idle_timeout_on_submit=True,
    )

    assert process._pty_process is not None
    kwargs = recorded["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["relay_terminal_input"] is True
    assert kwargs["arm_idle_timeout_on_submit"] is True


def test_running_process_pseudo_terminal_forwards_terminal_input_relay_options(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    recorded: dict[str, object] = {}

    class FakePtyProcess:
        def __init__(self, command: str | list[str], **kwargs: object) -> None:
            recorded["command"] = command
            recorded["kwargs"] = kwargs

    monkeypatch.setattr(running_process_module, "PseudoTerminalProcess", FakePtyProcess)

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
        relay_terminal_input=True,
        arm_idle_timeout_on_submit=True,
    )

    kwargs = recorded["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["relay_terminal_input"] is True
    assert kwargs["arm_idle_timeout_on_submit"] is True
    assert isinstance(process, FakePtyProcess)


def test_pseudo_terminal_windows_native_input_relay_forwards_events_and_arms_idle_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captures: list[FakeCapture] = []

    class FakeCapture:
        def __init__(self) -> None:
            self.started = False
            self.closed = False
            self._reads = 0
            captures.append(self)

        def start(self) -> None:
            self.started = True

        def read_batch(self, timeout: float | None = None) -> tuple[bytes, bool]:
            del timeout
            self._reads += 1
            if self._reads == 1:
                return (b"hello\r", True)
            raise TimeoutError

        def close(self) -> None:
            self.closed = True

    class FakeProc:
        def __init__(self, owner: PseudoTerminalProcess) -> None:
            self.owner = owner
            self.pid = 1234

        def poll(self) -> int | None:
            if self.owner._terminal_input_stop.is_set():
                return 0
            return None

    monkeypatch.setattr(pty_module, "NativeTerminalInput", FakeCapture)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc(process)  # type: ignore[assignment]
    process.idle_timeout_enabled = False
    writes: list[tuple[bytes, bool]] = []

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        raw = data.encode(process.encoding, process.errors) if isinstance(data, str) else data
        writes.append((raw, submit))
        process._terminal_input_stop.set()

    process.write = fake_write  # type: ignore[method-assign]
    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    deadline = time.time() + 1.0
    while process.terminal_input_relay_active and time.time() < deadline:
        time.sleep(0.01)

    capture = captures[0]
    process.stop_terminal_input_relay()

    assert capture.started is True
    assert capture.closed is True
    assert writes == [(b"hello\r", True)]
    assert process.idle_timeout_enabled is True


def test_pseudo_terminal_windows_native_process_relay_api_owns_relay_lifecycle(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class FakeProc:
        def __init__(self) -> None:
            self.started = False
            self.stopped = False
            self.active_checks = 0
            self.input_bytes_total = 0
            self.newline_events_total = 0
            self.submit_events_total = 0

        def start_terminal_input_relay(self) -> None:
            self.started = True

        def stop_terminal_input_relay(self) -> None:
            self.stopped = True

        def terminal_input_relay_active(self) -> bool:
            if not self.started:
                return False
            self.active_checks += 1
            if self.active_checks == 1:
                self.input_bytes_total = 6
                self.newline_events_total = 1
                self.submit_events_total = 1
                return True
            return False

        def pty_input_bytes_total(self) -> int:
            return self.input_bytes_total

        def pty_newline_events_total(self) -> int:
            return self.newline_events_total

        def pty_submit_events_total(self) -> int:
            return self.submit_events_total

    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(
        pty_module,
        "NativeTerminalInput",
        lambda: (_ for _ in ()).throw(AssertionError("python relay should stay unused")),
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc()  # type: ignore[assignment]
    process.idle_timeout_enabled = False

    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    assert process.terminal_input_relay_active is True
    assert process.idle_timeout_enabled is True
    assert process._pty_input_bytes_total == 6
    assert process._pty_newline_events_total == 1
    assert process._pty_submit_events_total == 1
    assert process.terminal_input_relay_active is False

    process.stop_terminal_input_relay()

    assert process._proc.started is True  # type: ignore[union-attr]
    assert process._proc.stopped is True  # type: ignore[union-attr]


def test_pseudo_terminal_windows_native_input_relay_preserves_shift_enter_vs_enter(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captures: list[FakeCapture] = []

    class FakeCapture:
        def __init__(self) -> None:
            self.started = False
            self.closed = False
            self._batches = iter(
                [
                    # read_batch merges all queued events in Rust; the relay
                    # receives a single merged batch per call.
                    (b"hello\x1b[13;2uworld\r", True),
                ]
            )
            captures.append(self)

        def start(self) -> None:
            self.started = True

        def read_batch(self, timeout: float | None = None) -> tuple[bytes, bool]:
            del timeout
            try:
                return next(self._batches)
            except StopIteration as exc:
                raise TimeoutError from exc

        def close(self) -> None:
            self.closed = True

    class FakeProc:
        def __init__(self, owner: PseudoTerminalProcess) -> None:
            self.owner = owner
            self.pid = 1234

        def poll(self) -> int | None:
            if self.owner._terminal_input_stop.is_set():
                return 0
            return None

    monkeypatch.setattr(pty_module, "NativeTerminalInput", FakeCapture)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc(process)  # type: ignore[assignment]
    process.idle_timeout_enabled = False
    writes: list[tuple[bytes, bool]] = []

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        raw = data.encode(process.encoding, process.errors) if isinstance(data, str) else data
        writes.append((raw, submit))
        process._terminal_input_stop.set()

    process.write = fake_write  # type: ignore[method-assign]
    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    deadline = time.time() + 1.0
    while process.terminal_input_relay_active and time.time() < deadline:
        time.sleep(0.01)

    capture = captures[0]
    process.stop_terminal_input_relay()

    assert capture.started is True
    assert capture.closed is True
    # All events batched into a single write; shift-enter sequence preserved.
    assert writes == [(b"hello\x1b[13;2uworld\r", True)]
    assert process.idle_timeout_enabled is True


def test_pseudo_terminal_idle_sampling_uses_native_process_metrics() -> None:
    class FakeMetrics:
        def prime(self) -> None:
            return None

        def sample(self) -> tuple[bool, float, int, int]:
            return (True, 7.5, 4096, 0)

    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], auto_run=False)
    process._native_process_metrics = FakeMetrics()
    process._pty_input_bytes_total = 2
    process._pty_output_bytes_total = 3
    process._pty_control_churn_bytes_total = 1

    sample = process._sample_idle_snapshot(ProcessIdleDetection())

    assert sample.process_alive is True
    assert sample.cpu_percent == 7.5
    assert sample.disk_io_bytes == 4096
    assert sample.network_io_bytes == 0


def test_windows_terminal_input_bytes_preserves_explicit_crlf() -> None:
    assert native_windows_terminal_input_bytes(b"a\r\nb") == b"a\r\nb"
    expected = b"a\rb" if sys.platform == "win32" else b"a\nb"
    assert native_windows_terminal_input_bytes(b"a\nb") == expected


def test_pseudo_terminal_can_set_positive_nice() -> None:
    if sys.platform == "win32":
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                f"{windows_priority_class_script()}\nimport time\ntime.sleep(0.3)",
            ],
            text=True,
            nice=5,
        )
        output = _read_until_contains(process, str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS))
        assert str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS) in output
        assert process.wait(timeout=5) == 0
        return

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import os, time; time.sleep(0.3); print(os.nice(0), flush=True)"],
        text=True,
        nice=5,
    )
    output = _read_until_contains(process, "5")
    assert int(output.strip().splitlines()[-1]) >= 5
    assert process.wait(timeout=5) == 0


def test_posix_pty_command_wraps_nice_before_exec(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(pty_module.sys, "platform", "darwin")

    command = pty_module._pty_command(["python", "-c", "print('x')"], False, 5)

    assert command[0] == sys.executable
    assert command[1:4] == [
        "-c",
        "import os, sys\n"
        "os.setpriority(os.PRIO_PROCESS, 0, int(sys.argv[1]))\n"
        "os.execvp(sys.argv[2], sys.argv[2:])\n",
        "5",
    ]
    assert command[4:] == ["python", "-c", "print('x')"]


def test_pseudo_terminal_accepts_priority_enum() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import os, time; time.sleep(0.3); print(os.nice(0), flush=True)"]
        if sys.platform != "win32"
        else [
            sys.executable,
            "-c",
            f"{windows_priority_class_script()}\nimport time\ntime.sleep(0.3)",
        ],
        text=True,
        nice=CpuPriority.LOW,
    )
    if sys.platform == "win32":
        output = _read_until_contains(process, str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS))
        assert str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS) in output
    else:
        output = _read_until_contains(process, "5")
        assert int(output.strip().splitlines()[-1]) >= 5
    assert process.wait(timeout=5) == 0


def test_interactive_kill_waits_for_exit() -> None:
    process = RunningProcess.interactive(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        mode=InteractiveMode.CONSOLE_SHARED,
    )
    process.kill()
    assert process.poll() is not None


def test_interactive_wait_raises_keyboard_interrupt_on_sigint() -> None:
    process = RunningProcess.interactive(
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
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )
    time.sleep(0.5)
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait(timeout=2)


def test_interactive_can_set_positive_nice() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        output_path = Path(temp_dir) / "nice.txt"
        if sys.platform == "win32":
            script = windows_priority_class_script(output_path=output_path)
            expected = WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        else:
            script = (
                "from pathlib import Path\n"
                "import os\n"
                "import time\n"
                "time.sleep(0.3)\n"
                f"Path(r'{output_path}').write_text(str(os.nice(0)), encoding='utf-8')\n"
            )
            expected = 5

        process = RunningProcess.interactive(
            [sys.executable, "-c", script],
            mode=InteractiveMode.CONSOLE_SHARED,
            nice=5,
        )
        assert process.wait(timeout=5) == 0
        observed = int(output_path.read_text(encoding="utf-8"))
        assert observed >= expected


def test_interactive_accepts_priority_enum() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        output_path = Path(temp_dir) / "nice.txt"
        if sys.platform == "win32":
            script = windows_priority_class_script(output_path=output_path)
            expected = WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        else:
            script = (
                "from pathlib import Path\n"
                "import os\n"
                "import time\n"
                "time.sleep(0.3)\n"
                "Path(r'"
                f"{output_path}"
                "').write_text(str(os.nice(0)), encoding='utf-8')\n"
            )
            expected = 5

        process = RunningProcess.interactive(
            [sys.executable, "-c", script],
            mode=InteractiveMode.CONSOLE_SHARED,
            nice=CpuPriority.LOW,
        )
        assert process.wait(timeout=5) == 0
        observed = int(output_path.read_text(encoding="utf-8"))
        assert observed >= expected


def test_running_process_exposes_interactive_launch_spec() -> None:
    spec = RunningProcess.interactive_launch_spec("console_isolated")
    assert spec.mode is InteractiveMode.CONSOLE_ISOLATED
    assert spec.ctrl_c_owner == "parent"


def test_console_isolated_uses_process_group_on_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, object] = {}

    class FakeProc:
        pid = 1234

        def __init__(self, command: object, **kwargs: object) -> None:
            captured["command"] = command
            captured.update(kwargs)

        def poll(self) -> int | None:
            return 0

        def start(self) -> None:
            return None

    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(pty_module, "NativeProcess", FakeProc)

    process = InteractiveProcess(
        [sys.executable, "-c", "print('x')"],
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )

    assert captured["create_process_group"] is True
    assert process.pid == 1234


def test_console_isolated_send_interrupt_uses_killpg_on_posix(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[int, int]] = []
    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(
        pty_module.os, "killpg", lambda pid, sig: calls.append((pid, sig)), raising=False
    )
    monkeypatch.setattr(pty_module.signal, "SIGINT", 2, raising=False)

    process = InteractiveProcess(
        [sys.executable, "-c", "print('x')"],
        mode=InteractiveMode.CONSOLE_ISOLATED,
        auto_run=False,
    )
    process._proc = SimpleNamespace(pid=4321)
    process.send_interrupt()

    assert calls == [(4321, pty_module.signal.SIGINT)]


def test_pseudo_terminal_kill_uses_killpg_on_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[int, int]] = []
    state = {"alive": True}
    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(pty_module.Pty, "is_available", classmethod(lambda cls: True))
    monkeypatch.setattr(
        pty_module.os,
        "killpg",
        lambda pid, sig: (calls.append((pid, sig)), state.__setitem__("alive", False)),
        raising=False,
    )
    monkeypatch.setattr(pty_module.signal, "SIGKILL", 9, raising=False)

    class FakeProc:
        pid = 2468

        def poll(self) -> int | None:
            return None if state["alive"] else -9

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = FakeProc()
    process.kill()

    assert calls == [(2468, pty_module.signal.SIGKILL)]


def test_pseudo_terminal_send_interrupt_delegates_to_native_process() -> None:
    calls: list[str] = []
    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = SimpleNamespace(send_interrupt=lambda: calls.append("interrupt"))

    process.send_interrupt()

    assert calls == ["interrupt"]
    assert process.interrupt_count == 1
    assert process.interrupted_by_caller is True


def test_running_process_use_pty_remains_constructor_compatible() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('pty compat')"],
        use_pty=True,
        capture=True,
    )
    assert process.wait(timeout=5) == 0
    assert b"pty compat" in process.stdout
    assert process.stderr == b""
