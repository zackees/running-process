import sys
from types import SimpleNamespace

import pytest

from running_process.pty import _console_io


class _Buffer:
    def __init__(self, *, fail: bool = False) -> None:
        self.values: list[bytes] = []
        self.fail = fail

    def write(self, value: bytes) -> None:
        if self.fail:
            self.fail = False
            raise UnicodeEncodeError("ascii", "x", 0, 1, "no")
        self.values.append(value)


class _Stream:
    encoding = "ascii"

    def __init__(self, *, fail_text: bool = False, buffer: _Buffer | None = None) -> None:
        self.values: list[str] = []
        self.fail_text = fail_text
        self.flushed = 0
        if buffer is not None:
            self.buffer = buffer

    def write(self, value: str) -> None:
        if self.fail_text:
            self.fail_text = False
            raise UnicodeEncodeError("ascii", value, 0, 1, "no")
        self.values.append(value)

    def flush(self) -> None:
        self.flushed += 1


def test_safe_console_write_handles_text_bytes_and_encoding_fallback(monkeypatch) -> None:
    monkeypatch.setattr(_console_io, "_ensure_windows_vt_output", lambda _stream: None)
    stream = _Stream()
    _console_io._safe_console_write(stream, "text")
    _console_io._safe_console_write(stream, b"byte\xff")
    assert stream.values == ["text", "\n", "byte�", "\n"]

    buffer = _Buffer()
    stream = _Stream(fail_text=True, buffer=buffer)
    _console_io._safe_console_write(stream, "café")
    assert buffer.values == [b"caf?\n"]

    stream = _Stream(fail_text=True)
    _console_io._safe_console_write(stream, "café")
    assert stream.values == ["caf?", "\n"]


def test_windows_console_output_handle_platform_and_error_paths(monkeypatch) -> None:
    stream = SimpleNamespace(fileno=lambda: 7)
    monkeypatch.setattr(_console_io.sys, "platform", "linux")
    assert _console_io._windows_console_output_handle(stream) is None

    monkeypatch.setattr(_console_io.sys, "platform", "win32")
    assert _console_io._windows_console_output_handle(SimpleNamespace()) is None
    monkeypatch.setitem(sys.modules, "msvcrt", SimpleNamespace(get_osfhandle=lambda fd: fd + 10))
    assert _console_io._windows_console_output_handle(stream) == 17

    def fail(_fd):
        raise OSError("bad descriptor")

    monkeypatch.setitem(sys.modules, "msvcrt", SimpleNamespace(get_osfhandle=fail))
    assert _console_io._windows_console_output_handle(stream) is None


@pytest.mark.parametrize(
    ("get_result", "initial_mode", "set_result", "expected"),
    [(0, 0, 1, False), (1, 5, 0, True), (1, 0, 1, True), (1, 0, 0, False)],
)
def test_enable_windows_vt_output_handle(
    monkeypatch: pytest.MonkeyPatch,
    get_result: int,
    initial_mode: int,
    set_result: int,
    expected: bool,
) -> None:
    monkeypatch.setattr(_console_io.sys, "platform", "win32")

    class Value:
        def __init__(self, value=0) -> None:
            self.value = value

    class Kernel:
        def GetConsoleMode(self, _handle, mode) -> int:
            mode.value = initial_mode
            return get_result

        def SetConsoleMode(self, _handle, _mode) -> int:
            return set_result

    fake_ctypes = SimpleNamespace(
        windll=SimpleNamespace(kernel32=Kernel()),
        c_uint32=Value,
        c_void_p=Value,
        byref=lambda value: value,
    )
    monkeypatch.setitem(sys.modules, "ctypes", fake_ctypes)
    assert _console_io._enable_windows_vt_output_handle(3) is expected


def test_ensure_windows_vt_output_caches_success(monkeypatch) -> None:
    attempts: list[int] = []
    monkeypatch.setattr(_console_io, "_windows_console_output_handle", lambda _stream: 8)
    monkeypatch.setattr(
        _console_io,
        "_enable_windows_vt_output_handle",
        lambda handle: attempts.append(handle) or True,
    )
    monkeypatch.setattr(_console_io, "_WINDOWS_VT_OUTPUT_HANDLES", set())
    _console_io._ensure_windows_vt_output(object())
    _console_io._ensure_windows_vt_output(object())
    assert attempts == [8]

    monkeypatch.setattr(_console_io, "_windows_console_output_handle", lambda _stream: None)
    _console_io._ensure_windows_vt_output(object())


def test_safe_console_write_chunk_uses_binary_and_text_fallbacks(monkeypatch) -> None:
    monkeypatch.setattr(_console_io, "_ensure_windows_vt_output", lambda _stream: None)
    stream = _Stream(buffer=_Buffer())
    _console_io._safe_console_write_chunk(stream, b"raw", encoding="utf-8", errors="strict")
    assert stream.buffer.values == [b"raw"]
    _console_io._safe_console_write_chunk(stream, b"", encoding="utf-8", errors="strict")

    stream = _Stream(buffer=_Buffer(fail=True))
    _console_io._safe_console_write_chunk(stream, b"text", encoding="utf-8", errors="strict")
    assert stream.values == ["text"]

    stream = _Stream(fail_text=True, buffer=_Buffer(fail=True))
    _console_io._safe_console_write_chunk(
        stream, "café".encode(), encoding="utf-8", errors="strict"
    )
    assert stream.buffer.values == [b"caf?"]

    stream = _Stream(fail_text=True)
    _console_io._safe_console_write_chunk(
        stream, "café".encode(), encoding="utf-8", errors="strict"
    )
    assert stream.values == ["caf?"]
