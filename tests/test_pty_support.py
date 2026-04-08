from __future__ import annotations

import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

import psutil
import pytest

import running_process.pty as pty_module
from running_process import (
    CpuPriority,
    ExpectRule,
    InteractiveMode,
    RunningProcess,
)
from running_process.pty import (
    IdleWaitResult,
    InteractiveProcess,
    InterruptResult,
    PseudoTerminalProcess,
    Pty,
    interactive_launch_spec,
)


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


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
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


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_accepts_string_command_and_auto_splits() -> None:
    process = RunningProcess.pseudo_terminal(
        f'{sys.executable} -c "print(\'string command ok\')"',
        text=True,
    )
    output = _read_until_contains(process, "string command ok")
    assert "string command ok" in output
    assert process.wait(timeout=5) == 0


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_preserves_carriage_returns() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.write('first\\rsecond\\n'); "
                "sys.stdout.flush()"
            ),
        ],
        text=True,
    )
    output = _read_until_contains(process, "second")
    assert "\r" in output
    process.wait(timeout=5)


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
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


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
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


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_restoration_callback_runs_on_exit() -> None:
    restored: list[str] = []
    cleaned: list[str] = []
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "print('done')"],
        text=True,
        restore_callback=lambda: restored.append("restored"),
        cleanup_callback=cleaned.append,
    )
    assert process.wait(timeout=5) == 0
    assert restored == ["restored"]
    assert cleaned == ["exit"]


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_restoration_callback_runs_on_kill() -> None:
    restored: list[str] = []
    cleaned: list[str] = []
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        restore_callback=lambda: restored.append("restored"),
        cleanup_callback=cleaned.append,
    )
    process.kill()
    assert restored == ["restored"]
    assert cleaned == ["kill"]


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_timeout_kills_process_and_restores_once() -> None:
    restored: list[str] = []
    cleaned: list[str] = []
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        restore_callback=lambda: restored.append("restored"),
        cleanup_callback=cleaned.append,
    )
    with pytest.raises(TimeoutError):
        process.wait(timeout=0.1)
    assert restored == ["restored"]
    assert cleaned == ["kill"]


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_kill_and_terminate_are_idempotent_after_exit() -> None:
    process = RunningProcess.pseudo_terminal([sys.executable, "-c", "print('done')"], text=True)
    assert process.wait(timeout=5) == 0
    process.kill()
    process.terminate()


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_interrupt_and_wait_reports_second_interrupt_success() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import signal, sys, time\n"
                "count = {'value': 0}\n"
                "def handler(sig, frame):\n"
                "    count['value'] += 1\n"
                "    print(f'interrupt:{count[\"value\"]}', flush=True)\n"
                "    if count['value'] >= 2:\n"
                "        raise KeyboardInterrupt\n"
                "signal.signal(signal.SIGINT, handler)\n"
                "print('ready>', flush=True)\n"
                "time.sleep(30)\n"
            ),
        ],
        text=True,
    )
    process.expect("ready>", timeout=5)
    result = process.interrupt_and_wait(grace_timeout=0.2, second_interrupt=True)
    assert isinstance(result, InterruptResult)
    assert result.interrupt_count >= 2
    assert process.exit_reason == "interrupt"


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_wait_for_idle_ignores_noise_with_predicate() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('noise\\r')\n"
                "sys.stdout.flush()\n"
                "time.sleep(0.2)\n"
                "sys.stdout.write('noise\\r')\n"
                "sys.stdout.flush()\n"
                "time.sleep(0.4)\n"
            ),
        ],
        text=True,
    )
    result = process.wait_for_idle(
        0.1,
        timeout=1.0,
        activity_predicate=lambda chunk: "noise" not in chunk,
    )
    assert isinstance(result, IdleWaitResult)
    assert result.reason in {"idle", "exit"}


def test_running_process_interactive_launches_console_mode() -> None:
    cleaned: list[str] = []
    process = RunningProcess.interactive(
        [sys.executable, "-c", "print('interactive')"],
        mode=InteractiveMode.CONSOLE_SHARED,
        cleanup_callback=cleaned.append,
    )
    assert process.wait(timeout=5) == 0
    assert cleaned == ["exit"]


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_can_set_positive_nice() -> None:
    if sys.platform == "win32":
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                "import psutil, time; time.sleep(0.3); print(psutil.Process().nice(), flush=True)",
            ],
            text=True,
            nice=5,
        )
        output = _read_until_contains(process, str(psutil.BELOW_NORMAL_PRIORITY_CLASS))
        assert str(psutil.BELOW_NORMAL_PRIORITY_CLASS) in output
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


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_pseudo_terminal_accepts_priority_enum() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import os, time; time.sleep(0.3); print(os.nice(0), flush=True)"]
        if sys.platform != "win32"
        else [
            sys.executable,
            "-c",
            "import psutil, time; time.sleep(0.3); print(psutil.Process().nice(), flush=True)",
        ],
        text=True,
        nice=CpuPriority.LOW,
    )
    if sys.platform == "win32":
        output = _read_until_contains(process, str(psutil.BELOW_NORMAL_PRIORITY_CLASS))
        assert str(psutil.BELOW_NORMAL_PRIORITY_CLASS) in output
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
                "    time.sleep(30)\n"
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
            script = (
                "from pathlib import Path\n"
                "import time\n"
                "import psutil\n"
                "time.sleep(0.3)\n"
                "Path(r'"
                f"{output_path}"
                "').write_text(str(psutil.Process().nice()), encoding='utf-8')\n"
            )
            expected = psutil.BELOW_NORMAL_PRIORITY_CLASS
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
            script = (
                "from pathlib import Path\n"
                "import time\n"
                "import psutil\n"
                "time.sleep(0.3)\n"
                "Path(r'"
                f"{output_path}"
                "').write_text(str(psutil.Process().nice()), encoding='utf-8')\n"
            )
            expected = psutil.BELOW_NORMAL_PRIORITY_CLASS
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


def test_console_isolated_uses_new_session_on_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, object] = {}

    class FakeProc:
        pid = 1234

        def poll(self) -> int | None:
            return 0

    def fake_popen(command: object, **kwargs: object) -> FakeProc:
        captured["command"] = command
        captured.update(kwargs)
        return FakeProc()

    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(pty_module.subprocess, "Popen", fake_popen)

    process = InteractiveProcess(
        [sys.executable, "-c", "print('x')"],
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )

    assert captured["start_new_session"] is True
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
    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(pty_module.Pty, "is_available", classmethod(lambda cls: True))
    monkeypatch.setattr(
        pty_module.os, "killpg", lambda pid, sig: calls.append((pid, sig)), raising=False
    )
    monkeypatch.setattr(pty_module.signal, "SIGKILL", 9, raising=False)

    class FakeProc:
        pid = 2468

        def poll(self) -> int | None:
            return None

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = FakeProc()
    process.kill()

    assert calls == [(2468, pty_module.signal.SIGKILL)]


@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
def test_running_process_use_pty_remains_constructor_compatible() -> None:
    process = RunningProcess([sys.executable, "-c", "print('pty compat')"], use_pty=True)
    assert process.wait(timeout=5) == 0
    assert "pty compat" in process.stdout
    assert process.stderr == ""
