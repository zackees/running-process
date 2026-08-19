from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest

from running_process.exit_status import ProcessAbnormalExit
from running_process.pty import IdleWaitResult, WaitCheckpoint, WaitForResult
from running_process.running_process import _wait_methods


def _process(*, pty=None, proc=None):
    return SimpleNamespace(
        KEYBOARD_INTERRUPT_EXIT_CODES={130},
        _allows_child_ctrl_c_interruption=False,
        _end_time=None,
        _exit_status=None,
        _handle_timeout=MagicMock(side_effect=TimeoutError("timed out")),
        _on_complete=MagicMock(),
        _output_formatter=MagicMock(),
        _proc=proc or MagicMock(),
        _pty_process=pty,
        _start_time=10.0,
        drain_combined=MagicMock(return_value=[]),
        kill=MagicMock(),
        poll=MagicMock(return_value=0),
        timeout=2.0,
        wait_for_idle=MagicMock(),
    )


def test_echo_streams_and_finalize_cover_callback_and_console_paths() -> None:
    process = _process()
    process.drain_combined.return_value = [("stdout", b"bytes"), ("stderr", "text")]
    callback = MagicMock()
    _wait_methods.echo_streams(process, callback)
    assert callback.call_args_list[0].args == ("bytes",)
    assert callback.call_args_list[1].args == ("text",)

    with patch.object(_wait_methods, "_safe_console_write") as write:
        _wait_methods.echo_streams(process)
    assert [call.args[1] for call in write.call_args_list] == [b"bytes", "text"]

    _wait_methods.finalize_wait(process)
    process._output_formatter.end.assert_called_once_with()
    process._on_complete.assert_called_once_with()

    process._on_complete = None
    _wait_methods.finalize_wait(process)


def test_resolve_echo_callback_plain_custom_and_timestamped() -> None:
    process = _process()
    callback = MagicMock()
    assert _wait_methods.resolve_echo_callback(process, False, None) is None
    assert _wait_methods.resolve_echo_callback(process, callback, None) is callback

    timestamped = MagicMock()
    with patch.object(
        _wait_methods, "_make_timestamped_callback", return_value=timestamped
    ) as make:
        assert (
            _wait_methods.resolve_echo_callback(
                process,
                True,
                "%H:%M",
            )
            is timestamped
        )
    assert make.call_args.args[1:] == ("%H:%M", 10.0)

    process._start_time = None
    with (
        patch.object(_wait_methods.time, "time", return_value=25.0),
        patch.object(_wait_methods, "_make_timestamped_callback", return_value=timestamped) as make,
    ):
        _wait_methods.resolve_echo_callback(process, True, "elapsed")
    assert make.call_args.args[1:] == ("elapsed", 25.0)


def test_wait_routes_idle_and_pipe_success_abnormal_and_interrupt() -> None:
    idle = IdleWaitResult(None, True, "idle_timeout")
    process = _process()
    process.wait_for_idle.return_value = idle
    assert _wait_methods._wait_impl(process, idle_detector=lambda *_: False) is idle
    process._output_formatter.end.assert_called_once_with()

    for code in (0, 7, 130):
        proc = MagicMock()
        proc.wait.return_value = code
        process = _process(proc=proc)
        with patch.object(_wait_methods.RunningProcessManagerSingleton, "unregister") as unregister:
            if code == 7:
                with pytest.raises(ProcessAbnormalExit):
                    _wait_methods._wait_impl(process, raise_on_abnormal_exit=True)
            elif code == 130:
                with pytest.raises(KeyboardInterrupt):
                    _wait_methods._wait_impl(process)
            else:
                assert _wait_methods._wait_impl(process) == 0
        unregister.assert_called_once_with(process)
        process._output_formatter.end.assert_called_once_with()


def test_wait_pipe_echo_timeout_and_keyboard_interrupt_cleanup() -> None:
    proc = MagicMock()
    proc.wait.return_value = 0
    process = _process(proc=proc)
    process.poll.side_effect = [None, 0]
    callback = MagicMock()
    with (
        patch.object(_wait_methods, "echo_streams") as echo_streams,
        patch.object(_wait_methods.time, "sleep"),
    ):
        assert _wait_methods._wait_impl(process, echo=callback) == 0
    assert echo_streams.call_count == 2
    proc.wait.assert_called_once_with(timeout=0)

    proc.wait.side_effect = TimeoutError
    process = _process(proc=proc)
    with pytest.raises(TimeoutError):
        _wait_methods._wait_impl(process, timeout=0.25)
    process._handle_timeout.assert_called_once_with(0.25)

    process = _process()
    with (
        patch.object(_wait_methods, "_wait_impl", side_effect=KeyboardInterrupt),
        pytest.raises(KeyboardInterrupt),
    ):
        _wait_methods.wait(process)
    process.kill.assert_called_once_with()

    process = _process()
    process._allows_child_ctrl_c_interruption = True
    with (
        patch.object(_wait_methods, "_wait_impl", side_effect=KeyboardInterrupt),
        pytest.raises(KeyboardInterrupt),
    ):
        _wait_methods.wait(process)
    process.kill.assert_not_called()


def test_wait_pty_direct_and_echo_paths() -> None:
    pty = MagicMock()
    pty.wait.return_value = 4
    process = _process(pty=pty)
    assert _wait_methods._wait_impl(process) == 4
    pty.wait.assert_called_once_with(timeout=2.0, raise_on_abnormal_exit=False)

    pty = MagicMock()
    pty.wait.return_value = 0
    process = _process(pty=pty)
    process.poll.side_effect = [None, 0]
    with patch.object(_wait_methods.time, "sleep"):
        assert _wait_methods._wait_impl(process, echo=True) == 0
    assert pty._echo_to_console.call_count == 2
    pty.wait.assert_called_once_with(timeout=0)


def test_idle_expect_and_wait_for_delegation_and_state_updates() -> None:
    process = _process()
    with pytest.raises(NotImplementedError):
        _wait_methods._wait_for_idle_impl(process)
    with pytest.raises(NotImplementedError):
        _wait_methods._wait_for_expect_impl(process)
    with pytest.raises(NotImplementedError):
        _wait_methods._wait_for_impl(process)
    with pytest.raises(NotImplementedError):
        _wait_methods.checkpoint(process)

    idle = IdleWaitResult(0, False, "process_exit")
    expected = WaitForResult(0, True, "condition_met")
    checkpoint = WaitCheckpoint(9)
    pty = MagicMock()
    pty.wait_for_idle.return_value = idle
    pty.wait_for_expect.return_value = expected
    pty.wait_for.return_value = expected
    pty.checkpoint.return_value = checkpoint
    process = _process(pty=pty)
    callback = MagicMock()

    with (
        patch.object(_wait_methods, "echo_streams") as echo_streams,
        patch.object(_wait_methods.RunningProcessManagerSingleton, "unregister") as unregister,
    ):
        assert _wait_methods._wait_for_idle_impl(process, echo=callback) is idle
        assert _wait_methods._wait_for_expect_impl(process, echo=callback) is expected
        assert _wait_methods._wait_for_impl(process, lambda: True, echo=callback) is expected
    assert echo_streams.call_count == 3
    assert unregister.call_count == 3
    assert _wait_methods.checkpoint(process) is checkpoint

    process = _process(pty=pty)
    pty.wait_for_idle.return_value = IdleWaitResult(None, True, "idle_timeout")
    pty.wait_for_expect.return_value = WaitForResult(None, False, "timeout")
    pty.wait_for.return_value = WaitForResult(None, False, "timeout")
    assert _wait_methods._wait_for_idle_impl(process, echo=True).returncode is None
    assert _wait_methods._wait_for_expect_impl(process, echo=True).returncode is None
    assert _wait_methods._wait_for_impl(process, echo=True).returncode is None
    assert pty._echo_to_console.call_count >= 3


@pytest.mark.parametrize(
    "wrapper", [_wait_methods.wait_for_idle, _wait_methods.wait_for_expect, _wait_methods.wait_for]
)
def test_wait_wrappers_kill_on_keyboard_interrupt(wrapper) -> None:
    process = _process()
    target = {
        _wait_methods.wait_for_idle: "_wait_for_idle_impl",
        _wait_methods.wait_for_expect: "_wait_for_expect_impl",
        _wait_methods.wait_for: "_wait_for_impl",
    }[wrapper]
    with patch.object(_wait_methods, target, side_effect=KeyboardInterrupt):
        with pytest.raises(KeyboardInterrupt):
            wrapper(process)
    process.kill.assert_called_once_with()
