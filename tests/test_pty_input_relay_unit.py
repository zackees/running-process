import sys
import threading
from types import SimpleNamespace
from unittest.mock import MagicMock

from running_process.pty import _pseudo_terminal
from running_process.pty import _pty_input_relay as relay


class _ImmediateThread:
    def __init__(self, *, target, **_kwargs) -> None:
        self._target = target
        self.joined = False

    def start(self) -> None:
        self._target()

    def is_alive(self) -> bool:
        return True

    def join(self, *, timeout: float) -> None:
        assert timeout == 0.2
        self.joined = True


def _process(**overrides):
    values = {
        "_allows_child_ctrl_c_interruption": False,
        "_arm_idle_timeout_on_submit": False,
        "_ensure_started": MagicMock(),
        "_native_idle_detector": None,
        "_proc": MagicMock(),
        "_pty_input_bytes_total": 0,
        "_pty_newline_events_total": 0,
        "_pty_submit_events_total": 0,
        "_terminal_input_capture": None,
        "_terminal_input_restore_state": None,
        "_terminal_input_stop": threading.Event(),
        "_terminal_input_thread": None,
        "encoding": "utf-8",
        "errors": "strict",
        "idle_timeout_enabled": False,
        "last_activity_at": 0.0,
        "pid": 42,
        "poll": MagicMock(return_value=0),
        "terminal_input_relay_active": False,
        "write": MagicMock(),
    }
    values.update(overrides)
    return SimpleNamespace(**values)


def test_write_and_submit_update_metrics_and_native_detector(monkeypatch) -> None:
    detector = MagicMock()
    process = _process(_native_idle_detector=detector)
    monkeypatch.setattr(relay.time, "time", lambda: 12.5)
    monkeypatch.setattr(relay, "sync_native_input_metrics", MagicMock())

    relay.write(process, "line\n", submit=True)
    assert process._pty_input_bytes_total == 5
    assert process._pty_newline_events_total == 1
    assert process._pty_submit_events_total == 1
    assert process.last_activity_at == 12.5
    detector.record_input.assert_called_once_with(5)
    process._proc.write.assert_called_once_with(b"line\n", submit=True)

    process.write = MagicMock()
    relay.submit(process, b"answer")
    process.write.assert_called_once_with(b"answer", submit=True)


def test_active_prefers_windows_native_api_and_falls_back_to_thread(monkeypatch) -> None:
    native = MagicMock()
    native.terminal_input_relay_active.return_value = True
    process = _process(_proc=native)
    monkeypatch.setattr(_pseudo_terminal.sys, "platform", "win32")
    monkeypatch.setattr(relay, "sync_native_input_metrics", MagicMock())
    assert relay.terminal_input_relay_active(process)
    relay.sync_native_input_metrics.assert_called_once_with(process)

    monkeypatch.setattr(_pseudo_terminal.sys, "platform", "linux")
    process._terminal_input_thread = _ImmediateThread(target=lambda: None)
    assert relay.terminal_input_relay_active(process)
    process._terminal_input_thread = None
    assert not relay.terminal_input_relay_active(process)


def test_native_metric_sync_arms_only_for_new_submit_events() -> None:
    native = MagicMock()
    native.pty_input_bytes_total.return_value = 101
    native.pty_newline_events_total.return_value = 4
    native.pty_submit_events_total.return_value = 3
    process = _process(
        _proc=native,
        _arm_idle_timeout_on_submit=True,
        _pty_submit_events_total=1,
    )
    relay.sync_native_input_metrics(process)
    assert process._pty_input_bytes_total == 101
    assert process._pty_newline_events_total == 4
    assert process._pty_submit_events_total == 3
    assert process.idle_timeout_enabled

    process = _process(_proc=None, _arm_idle_timeout_on_submit=True)
    relay.sync_native_input_metrics(process)
    process = _process(_proc=native, _arm_idle_timeout_on_submit=False)
    relay.sync_native_input_metrics(process)
    native.pty_input_bytes_total.assert_called_once_with()


def test_idle_timeout_arming_requires_submit_when_configured() -> None:
    process = _process()
    relay.maybe_arm_idle_timeout_from_terminal_input(process, submit=False)
    assert not process.idle_timeout_enabled
    process._arm_idle_timeout_on_submit = True
    relay.maybe_arm_idle_timeout_from_terminal_input(process, submit=False)
    assert not process.idle_timeout_enabled
    relay.maybe_arm_idle_timeout_from_terminal_input(process, submit=True)
    assert process.idle_timeout_enabled
    relay.maybe_arm_idle_timeout_from_terminal_input(process, submit=True)


def test_windows_relay_uses_native_api_when_child_owns_it(monkeypatch) -> None:
    native = MagicMock()
    process = _process(_proc=native, _allows_child_ctrl_c_interruption=True)
    monkeypatch.setattr(relay, "sync_native_input_metrics", MagicMock())
    relay.start_windows_terminal_input_relay(process)
    native.start_terminal_input_relay.assert_called_once_with()
    relay.sync_native_input_metrics.assert_called_once_with(process)


def test_windows_python_relay_filters_ctrl_c_and_closes(monkeypatch) -> None:
    capture = MagicMock()
    capture.read_batch.side_effect = [TimeoutError, (b"\x03payload", True)]
    monkeypatch.setattr(_pseudo_terminal, "NativeTerminalInput", lambda: capture)
    monkeypatch.setattr(relay.threading, "Thread", _ImmediateThread)
    process = _process()
    process.poll.side_effect = [None, None, 0]

    relay.start_windows_terminal_input_relay(process)

    capture.start.assert_called_once_with()
    process.write.assert_called_once_with(b"payload", submit=True)
    capture.close.assert_called_once_with()


def test_posix_relay_skips_non_tty_and_drains_ready_input(monkeypatch) -> None:
    fake_termios = SimpleNamespace(
        TCSANOW=1,
        tcgetattr=MagicMock(return_value=["previous"]),
        tcsetattr=MagicMock(),
    )
    fake_tty = SimpleNamespace(setraw=MagicMock())
    ready = iter([([9], [], []), ([9], [], []), ([], [], [])])
    fake_select = SimpleNamespace(select=lambda *_args: next(ready))
    monkeypatch.setitem(sys.modules, "termios", fake_termios)
    monkeypatch.setitem(sys.modules, "tty", fake_tty)
    monkeypatch.setitem(sys.modules, "select", fake_select)

    stdin = SimpleNamespace(isatty=lambda: False)
    monkeypatch.setattr(_pseudo_terminal.sys, "stdin", stdin)
    process = _process()
    relay.start_posix_terminal_input_relay(process)
    assert process._terminal_input_thread is None

    stdin = SimpleNamespace(isatty=lambda: True, fileno=lambda: 9)
    monkeypatch.setattr(_pseudo_terminal.sys, "stdin", stdin)
    monkeypatch.setattr(relay.threading, "Thread", _ImmediateThread)
    reads = iter([b"part", b"\nrest"])
    monkeypatch.setattr(relay.os, "read", lambda *_args: next(reads))
    process = _process()
    process.poll.side_effect = [None, 0]

    relay.start_posix_terminal_input_relay(process)

    fake_tty.setraw.assert_called_once_with(9)
    process.write.assert_called_once_with(b"part\nrest", submit=True)
    fake_termios.tcsetattr.assert_called_once_with(9, 1, ["previous"])


def test_start_dispatch_and_stop_clean_every_resource(monkeypatch) -> None:
    process = _process(terminal_input_relay_active=True)
    relay.start_terminal_input_relay(process, arm_idle_timeout_on_submit=True)
    process._ensure_started.assert_called_once_with()

    process = _process()
    monkeypatch.setattr(_pseudo_terminal.sys, "platform", "win32")
    monkeypatch.setattr(relay, "start_windows_terminal_input_relay", MagicMock())
    relay.start_terminal_input_relay(process, arm_idle_timeout_on_submit=True)
    assert process._arm_idle_timeout_on_submit
    relay.start_windows_terminal_input_relay.assert_called_once_with(process)

    capture = MagicMock()
    thread = _ImmediateThread(target=lambda: None)
    native = MagicMock()
    process = _process(
        _proc=native,
        _terminal_input_capture=capture,
        _terminal_input_restore_state=None,
        _terminal_input_thread=thread,
    )
    monkeypatch.setattr(relay, "sync_native_input_metrics", MagicMock())
    monkeypatch.setattr(relay, "restore_posix_terminal_input", MagicMock())
    relay.stop_terminal_input_relay(process)
    assert process._terminal_input_stop.is_set()
    native.stop_terminal_input_relay.assert_called_once_with()
    assert thread.joined
    capture.close.assert_called_once_with()
    relay.restore_posix_terminal_input.assert_called_once_with(process)
