from __future__ import annotations

import sys

import pytest

from running_process import NativeTerminalInput, NativeTerminalInputEvent


def test_native_terminal_input_is_exported() -> None:
    capture = NativeTerminalInput()
    assert isinstance(capture, NativeTerminalInput)
    assert capture.capturing is False


def test_native_terminal_input_event_type_is_exported() -> None:
    assert NativeTerminalInputEvent.__name__ == "NativeTerminalInputEvent"


def test_native_terminal_input_empty_queue_and_close_contract() -> None:
    capture = NativeTerminalInput()
    assert capture.available() is False
    assert capture.original_console_mode is None
    assert capture.active_console_mode is None
    assert capture.drain() == []
    assert capture.drain_events() == []

    with pytest.raises(RuntimeError, match="closed"):
        capture.read_event_non_blocking()
    with pytest.raises(RuntimeError, match="closed"):
        capture.read_non_blocking()
    with pytest.raises(RuntimeError, match="closed"):
        capture.read(timeout=0)
    with pytest.raises(RuntimeError, match="closed"):
        capture.read_event(timeout=0)
    with pytest.raises(RuntimeError, match="closed"):
        capture.read_batch(timeout=0)

    capture.stop()
    capture.close()
    with pytest.raises(RuntimeError, match="closed"):
        capture.read_non_blocking()
    with pytest.raises(RuntimeError, match="closed"):
        capture.read_event_non_blocking()


@pytest.mark.skipif(sys.platform == "win32", reason="non-Windows contract")
def test_native_terminal_input_start_reports_platform_limitation() -> None:
    capture = NativeTerminalInput()
    with pytest.raises(RuntimeError, match="only available on Windows"):
        capture.start()
