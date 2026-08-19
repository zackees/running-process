import pytest

from running_process.pty._idle_helpers import (
    _build_default_idle_reset,
    _callable_arity,
    _compile_idle_detector,
    _condition_callback_arity,
    _control_churn_bytes,
    _default_idle_reset,
    _flush_wait_input,
    _input_contains_newline,
    _invoke_condition_callback,
    _invoke_wait_callback,
    _merge_idle_diff,
    _normalize_wait_conditions,
    _resolve_expect_offset,
    _start_event_count,
    _wait_callback_arity,
)
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleContext,
    IdleDecision,
    IdleDetection,
    IdleInfoDiff,
    IdleStartTrigger,
    ProcessIdleDetection,
    PtyIdleDetection,
    WaitCallbackResult,
    WaitCheckpoint,
)
from running_process.pty._wait_input import _BufferedInput


class _FakeProcess:
    output = "hello"
    encoding = "utf-8"
    errors = "strict"
    _pty_newline_events_total = 3
    _pty_submit_events_total = 4

    def __init__(self) -> None:
        self.writes: list[tuple[object, bool | None]] = []
        self.synced = 0

    def write(self, data, *, submit=None) -> None:
        self.writes.append((data, submit))

    def _sync_native_input_metrics(self) -> None:
        self.synced += 1


def _diff(**updates) -> IdleInfoDiff:
    values = {"delta_seconds": 1.0, "process_alive": True}
    values.update(updates)
    return IdleInfoDiff(**values)


def test_idle_detector_compilation_and_arity_validation() -> None:
    assert _compile_idle_detector(None) == (None, None, None)

    timing, callback, predicate = _compile_idle_detector(IdleDetection())
    assert timing is not None
    assert callback is None
    assert predicate is not None

    def reached(_diff):
        return IdleDecision.IS_IDLE

    timing, callback, predicate = _compile_idle_detector(
        IdleDetection(idle_reached=reached)
    )
    assert timing is not None
    assert callback is reached
    assert predicate is None

    def reset(_diff, _context):
        return True

    timing, callback, predicate = _compile_idle_detector(reset)
    assert timing is not None
    assert callback is None
    assert predicate is reset
    assert _callable_arity(lambda _one: None) == 1
    assert _callable_arity(lambda _one, _two: None) == 2
    assert _callable_arity(lambda _one, *args: None) == 1
    assert _callable_arity(lambda _one, _two, *args: None) == 2
    timing, callback, predicate = _compile_idle_detector(reached)
    assert timing is not None
    assert callback is reached
    assert predicate is None

    with pytest.raises(ValueError, match="mutually exclusive"):
        _compile_idle_detector(IdleDetection(idle_reached=reached, predicate=reset))
    with pytest.raises(TypeError, match="1 or 2"):
        _compile_idle_detector(lambda: None)
    with pytest.raises(TypeError, match="idle_detector must be"):
        _compile_idle_detector("bad")  # type: ignore[arg-type]


def test_wait_and_condition_callbacks_receive_buffers_and_process() -> None:
    process = _FakeProcess()
    assert _wait_callback_arity(lambda: None) == 0
    assert _wait_callback_arity(lambda _buffer: None) == 1
    assert _wait_callback_arity(lambda _buffer, _process: None) == 2
    assert _wait_callback_arity(lambda *args: None) == 0
    with pytest.raises(TypeError, match="0, 1, or 2"):
        _wait_callback_arity(lambda _a, _b, _c: None)

    assert _invoke_wait_callback(lambda: "zero", process) == ("zero", [])

    def one(buffer):
        buffer.write("one")
        return 1

    assert _invoke_wait_callback(one, process) == (1, ["one"])

    def two(buffer, received_process):
        assert received_process is process
        buffer.submit(b"two")
        return 2

    result, items = _invoke_wait_callback(two, process)
    assert result == 2
    assert items == [_BufferedInput(b"two", submit=True)]

    for arity in range(4):
        callback = [
            lambda: WaitCallbackResult.EXIT,
            lambda _payload: WaitCallbackResult.EXIT,
            lambda _payload, _buffer: WaitCallbackResult.EXIT,
            lambda _payload, _buffer, _process: WaitCallbackResult.EXIT,
        ][arity]
        assert _condition_callback_arity(callback) == arity
        assert _invoke_condition_callback(callback, "payload", process)[0] is (
            WaitCallbackResult.EXIT
        )
    assert _condition_callback_arity(lambda *args: None) == 0
    with pytest.raises(TypeError, match="0, 1, 2, or 3"):
        _condition_callback_arity(lambda _a, _b, _c, _d: None)
    with pytest.raises(TypeError, match="must return"):
        _invoke_condition_callback(lambda: "bad", None, process)


def test_wait_condition_normalization_and_input_flush() -> None:
    def callback():
        return None

    conditions = [Idle(), Expect("ready"), Callback(callback)]
    normalized = _normalize_wait_conditions(
        conditions[0], callback, [conditions[1]], (conditions[2], callback)
    )
    assert len(normalized) == 5
    with pytest.raises(TypeError, match="conditions must"):
        _normalize_wait_conditions([object()])  # type: ignore[list-item]
    with pytest.raises(TypeError, match="conditions must"):
        _normalize_wait_conditions(object())  # type: ignore[arg-type]

    process = _FakeProcess()
    _flush_wait_input(process, ["a", b"b", _BufferedInput("c", submit=True)])
    assert process.writes == [("a", None), (b"b", None), ("c", True)]


def test_offsets_start_events_idle_reset_and_diff_merge() -> None:
    process = _FakeProcess()
    assert _resolve_expect_offset(Expect("x", after="start"), process) == 0
    assert _resolve_expect_offset(Expect("x", after="now"), process) == 5
    assert _resolve_expect_offset(Expect("x", after=WaitCheckpoint(-2)), process) == 0

    assert _start_event_count(process, IdleStartTrigger.INPUT_NEWLINE) == 3
    assert _start_event_count(process, IdleStartTrigger.INPUT_SUBMIT) == 4
    assert _start_event_count(process, IdleStartTrigger.IMMEDIATE) == 1
    assert process.synced == 3

    context = IdleContext(0.0, 0.0, 1)
    assert _default_idle_reset(
        _diff(pty_input_bytes=1), context, IdleDetection()
    )
    assert _default_idle_reset(
        _diff(pty_control_churn_bytes=1), context, IdleDetection()
    )
    assert not _default_idle_reset(
        _diff(pty_control_churn_bytes=1),
        context,
        IdleDetection(pty=PtyIdleDetection(count_control_churn_as_output=False)),
    )
    process_cfg = ProcessIdleDetection()
    cfg = IdleDetection(pty=None, process=process_cfg)
    assert _default_idle_reset(_diff(cpu_percent=3), context, cfg)
    assert _default_idle_reset(_diff(disk_io_bytes=5000), context, cfg)
    assert _default_idle_reset(_diff(network_io_bytes=5000), context, cfg)
    assert not _default_idle_reset(_diff(), context, cfg)
    assert not _build_default_idle_reset(cfg)(_diff(), context)
    assert _input_contains_newline(b"line\n")
    assert _input_contains_newline(b"line\r")
    assert not _input_contains_newline(b"line")

    merged = _merge_idle_diff(
        _diff(delta_seconds=1, cpu_percent=10, pty_input_bytes=2),
        _diff(delta_seconds=3, cpu_percent=30, pty_input_bytes=4),
    )
    assert merged.delta_seconds == 4
    assert merged.cpu_percent == 25
    assert merged.pty_input_bytes == 6
    assert _merge_idle_diff(_diff(delta_seconds=0), _diff(delta_seconds=0)).cpu_percent == 0


def test_control_churn_counts_complete_and_partial_sequences() -> None:
    assert _control_churn_bytes(b"plain") == 0
    assert _control_churn_bytes(b"\x08\r\x7f") == 3
    assert _control_churn_bytes(b"a\x1b[31mb") == 5
    assert _control_churn_bytes(b"\x1b[") == 2
    assert _control_churn_bytes(b"\x1b7") == 1
