from running_process.running_process._types import (
    EOS,
    CapturedProcessStream,
    ProcessInfo,
    ProcessOutputEvent,
)


class _FakeProcess:
    encoding = "utf-8"
    errors = "strict"

    def __init__(self, value: str | bytes) -> None:
        self.value = value

    def has_pending_output(self) -> bool:
        return True

    def has_pending_stdout(self) -> bool:
        return False

    def has_pending_stderr(self) -> bool:
        return True

    def _captured_stream_value(self, _stream: str) -> str | bytes:
        return self.value

    def drain_stdout(self) -> list[str]:
        return ["stdout"]

    def drain_stderr(self) -> list[str]:
        return ["stderr"]

    def drain_combined(self) -> list[tuple[str, str]]:
        return [("stdout", "combined")]


def test_process_output_value_types() -> None:
    assert repr(EOS) == "EOS"
    assert ProcessInfo(12, ["cmd"], 1.5).pid == 12
    assert not ProcessOutputEvent("line", None, 0).streams_drained
    assert not ProcessOutputEvent(EOS, None, None).finished_and_drained
    assert ProcessOutputEvent(EOS, EOS, 0).finished_and_drained


def test_captured_process_stream_delegates_text_operations() -> None:
    process = _FakeProcess("hello")
    combined = CapturedProcessStream(process, "combined")  # type: ignore[arg-type]
    stdout = CapturedProcessStream(process, "stdout")  # type: ignore[arg-type]
    stderr = CapturedProcessStream(process, "stderr")  # type: ignore[arg-type]

    assert combined.available()
    assert not stdout.available()
    assert stderr.available()
    assert combined.read() == "hello"
    assert combined.drain() == [("stdout", "combined")]
    assert stdout.drain() == ["stdout"]
    assert stderr.drain() == ["stderr"]
    assert repr(combined) == "'hello'"
    assert str(combined) == "hello"
    assert bytes(combined) == b"hello"
    assert combined == "hello"
    assert combined
    assert len(combined) == 5
    assert "ell" in combined
    assert combined.upper() == "HELLO"


def test_captured_process_stream_decodes_and_preserves_bytes() -> None:
    stream = CapturedProcessStream(_FakeProcess(b"bytes"), "combined")  # type: ignore[arg-type]

    assert str(stream) == "bytes"
    assert bytes(stream) == b"bytes"
