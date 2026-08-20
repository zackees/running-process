from __future__ import annotations

import threading
import unittest
from unittest.mock import MagicMock, patch

from running_process.exit_status import ProcessAbnormalExit
from running_process.expect import ExpectMatch
from running_process.pty import _interactive, _pty_expect, _pty_idle_waiter, _pty_wait_for
from running_process.pty._idle_state import _IdleSample
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleDecision,
    IdleDetection,
    IdleTiming,
    IdleWaitResult,
    InteractiveMode,
    ProcessIdleDetection,
    WaitForResult,
)


def sample(at: float, *, returncode: int | None = None) -> _IdleSample:
    return _IdleSample(
        sampled_at=at,
        process_alive=returncode is None,
        pty_input_bytes=int(at),
        pty_output_bytes=int(at * 2),
        pty_control_churn_bytes=int(at),
        cpu_percent=0.0,
        disk_io_bytes=0,
        network_io_bytes=0,
        returncode=returncode,
    )


def idle_process() -> MagicMock:
    process = MagicMock()
    process._registered_idle_detector = None
    process.idle_timeout_enabled = True
    process.last_activity_at = 0.0
    process.capture = False
    process.poll.return_value = None
    process._sample_idle_snapshot.side_effect = [sample(0.0), sample(1.0)]
    return process


class PtyIdleWaiterCoverageTest(unittest.TestCase):
    def test_no_detector_delegates_to_wait(self) -> None:
        process = idle_process()
        process.wait.return_value = 7
        result = _pty_idle_waiter.wait_for_idle(process, timeout=0.1)
        self.assertEqual(result.returncode, 7)
        self.assertFalse(result.idle_detected)
        process.wait.assert_called_once()

    def test_callback_idle_exit_and_invalid_decision_paths(self) -> None:
        detector = IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.0,
                stability_window_seconds=0.0,
                sample_interval_seconds=0.0,
            ),
            idle_reached=lambda _diff: IdleDecision.IS_IDLE,
        )
        process = idle_process()
        result = _pty_idle_waiter.wait_for_idle(process, detector)
        self.assertTrue(result.idle_detected)
        self.assertEqual(result.exit_reason, "idle_timeout")

        invalid = IdleDetection(
            timing=IdleTiming(sample_interval_seconds=0.0),
            idle_reached=lambda _diff: "invalid",  # type: ignore[arg-type,return-value]
        )
        process = idle_process()
        with self.assertRaises(TypeError):
            _pty_idle_waiter.wait_for_idle(process, invalid)

        exiting = IdleDetection(
            timing=IdleTiming(sample_interval_seconds=0.0),
            predicate=lambda _diff, _ctx: True,
        )
        process = idle_process()
        process._sample_idle_snapshot.side_effect = [sample(0.0), sample(1.0, returncode=0)]
        result = _pty_idle_waiter.wait_for_idle(process, exiting)
        self.assertEqual(result.exit_reason, "process_exit")
        process._finalize.assert_called_once_with("exit")

    def test_sample_snapshot_uses_native_metrics_and_atomic_pty_counters(self) -> None:
        process = MagicMock()
        process._native_process_metrics.sample.return_value = (True, 2.5, 10, 20)
        process._pty_input_bytes_total = 3
        process._pty_output_bytes_total = 4
        process._pty_control_churn_bytes_total = 5
        process._proc.pty_output_bytes_total.return_value = 6
        process._proc.pty_control_churn_bytes_total.return_value = 7
        process.poll.return_value = None
        result = _pty_idle_waiter.sample_idle_snapshot(
            process, ProcessIdleDetection()
        )
        self.assertTrue(result.process_alive)
        self.assertEqual(result.cpu_percent, 2.5)
        self.assertEqual(result.pty_output_bytes, 6)
        self.assertEqual(result.pty_control_churn_bytes, 7)

        process._proc.pty_output_bytes_total.side_effect = AttributeError()
        process._proc.pty_control_churn_bytes_total.side_effect = AttributeError()
        process.poll.return_value = 4
        result = _pty_idle_waiter.sample_idle_snapshot(process, None)
        self.assertFalse(result.process_alive)
        self.assertEqual(result.pty_output_bytes, 4)
        self.assertEqual(result.returncode, 4)

    def test_native_wait_and_exit_watcher_cover_returncode_and_none_detector(self) -> None:
        process = MagicMock()
        process.last_activity_at = 1.0
        process._idle_timeout_signal._native = object()
        process.poll.return_value = 0
        process.pid = 9
        process._native_idle_detector = None
        process._native_exit_watcher = None
        _pty_idle_waiter._start_native_exit_watcher(process)
        self.assertIsNone(process._native_exit_watcher)

        native = MagicMock()
        native.wait.return_value = (True, "idle_timeout", 1.5, 0)
        detector = IdleDetection(timing=IdleTiming(1.0, 0.1, 0.01))
        with patch.object(_pty_idle_waiter, "NativeIdleDetector", return_value=native):
            result = _pty_idle_waiter.wait_for_idle_native(
                process, detector, timeout=2.0
            )
        self.assertTrue(result.idle_detected)
        self.assertEqual(result.returncode, 0)
        process._native_exit_watcher.join(timeout=1.0)
        self.assertFalse(process._native_exit_watcher.is_alive())
        process._drain_native_until_eof.assert_called_once()
        process._finalize.assert_called_once_with("exit")


class PtyWaitForCoverageTest(unittest.TestCase):
    def test_empty_single_idle_and_expect_without_capture_shortcuts(self) -> None:
        process = MagicMock()
        process.wait.return_value = 3
        result = _pty_wait_for.wait_for(process)
        self.assertEqual(result.exit_reason, "process_exit")
        self.assertEqual(result.returncode, 3)

        idle_result = IdleWaitResult(
            returncode=None,
            idle_detected=True,
            exit_reason="idle_timeout",
            idle_for_seconds=2.0,
        )
        process.wait_for_idle.return_value = idle_result
        result = _pty_wait_for.wait_for(process, Idle())
        self.assertTrue(result.matched)
        self.assertEqual(result.exit_reason, "condition_met")

        process.capture = False
        with self.assertRaises(NotImplementedError):
            _pty_wait_for.wait_for(process, Expect("needle"), timeout=0.01)
        with self.assertRaises(ValueError):
            _pty_wait_for.wait_for(process, Idle(), Idle(), timeout=0.01)

    def test_expect_callback_and_timeout_paths_are_bounded(self) -> None:
        process = MagicMock()
        process.capture = True
        process.idle_timeout_enabled = True
        process._snapshot_output_history.return_value = ("hello world", 11)
        process._snapshot_output_since.return_value = ("", 11)
        process.poll.return_value = None
        result = _pty_wait_for.wait_for(process, Expect("world"), timeout=1.0)
        self.assertTrue(result.matched)
        self.assertEqual(result.expect_match.matched, "world")

        process = MagicMock()
        process.capture = False
        process.idle_timeout_enabled = True
        process.poll.return_value = None
        callback_called = threading.Event()

        def callback() -> bool:
            callback_called.set()
            return True

        result = _pty_wait_for.wait_for(
            process,
            Callback(callback, poll_interval_seconds=0.001),
            timeout=1.0,
        )
        self.assertTrue(callback_called.is_set())
        self.assertTrue(result.matched)

        process = MagicMock()
        process.capture = False
        process.idle_timeout_enabled = True
        process.poll.return_value = None
        result = _pty_wait_for.wait_for(
            process,
            Callback(lambda: False, poll_interval_seconds=0.001),
            timeout=0.001,
        )
        self.assertFalse(result.matched)
        self.assertEqual(result.exit_reason, "timeout")


class PtyExpectCoverageTest(unittest.TestCase):
    def test_wait_for_expect_registration_transitions(self) -> None:
        process = MagicMock()
        process.capture = False
        with self.assertRaises(NotImplementedError):
            _pty_expect.wait_for_expect(process)

        process.capture = True
        process._registered_expect_conditions = []
        with self.assertRaises(ValueError):
            _pty_expect.wait_for_expect(process)

        first = Expect("first")
        process._registered_expect_conditions = [first]
        process.wait_for.return_value = WaitForResult(
            returncode=None, matched=False, exit_reason="timeout"
        )
        result = _pty_expect.wait_for_expect(process)
        self.assertFalse(result.matched)
        self.assertEqual(process._registered_expect_conditions, [first])

        matched = ExpectMatch("first", "first", (0, 5), ())
        process.wait_for.return_value = WaitForResult(
            returncode=None,
            matched=True,
            exit_reason="condition_met",
            expect_match=matched,
        )
        result = _pty_expect.wait_for_expect(process)
        self.assertTrue(result.matched)
        self.assertEqual(process._registered_expect_conditions, [])

        process._registered_expect_conditions = [first]
        next_expect = Expect("second")
        _pty_expect.wait_for_expect(process, next_expect)
        registered = process._registered_expect_conditions[0]
        self.assertEqual(registered.pattern, "second")
        self.assertEqual(registered.after.offset, 5)

    def test_expect_match_chunk_timeout_and_eof_paths(self) -> None:
        process = MagicMock()
        process.capture = False
        with self.assertRaises(NotImplementedError):
            _pty_expect.expect(process, "x")

        process.capture = True
        process.encoding = "utf-8"
        process.errors = "replace"
        process._snapshot_output_history.return_value = ("already here", 12)
        match = _pty_expect.expect(process, "here")
        self.assertEqual(match.matched, "here")

        process._snapshot_output_history.return_value = ("", 0)
        process.read.return_value = b"new needle"
        process._buffer.history_bytes.return_value = 10
        match = _pty_expect.expect(process, "needle", timeout=1.0)
        self.assertEqual(match.matched, "needle")

        process._snapshot_output_history.return_value = ("", 0)
        process.read.side_effect = EOFError()
        with self.assertRaises(EOFError):
            _pty_expect.expect(process, "missing", timeout=1.0)

        process.read.side_effect = TimeoutError()
        process._snapshot_output_since.return_value = ("", 0)
        process.poll.return_value = None
        with patch.object(_pty_expect.time, "time", side_effect=[0.0, 1.0]):
            with self.assertRaises(TimeoutError):
                _pty_expect.expect(process, "missing", timeout=0.5)

        process.poll.return_value = 0
        with patch.object(_pty_expect.time, "time", side_effect=[0.0, 1.0]):
            with self.assertRaises(EOFError):
                _pty_expect.expect(process, "missing", timeout=0.5)


class InteractiveProcessCoverageTest(unittest.TestCase):
    def test_validation_start_poll_and_wait_errors(self) -> None:
        with self.assertRaises(ValueError):
            _interactive.InteractiveProcess(
                ["unused"], mode=InteractiveMode.PSEUDO_TERMINAL, auto_run=False
            )
        process = _interactive.InteractiveProcess(["unused"], auto_run=False)
        self.assertIsNone(process.poll())
        self.assertIsNone(process.pid)
        with self.assertRaises(RuntimeError):
            process.wait()
        with self.assertRaises(RuntimeError):
            process.terminate()
        with self.assertRaises(RuntimeError):
            process.kill()
        with self.assertRaises(RuntimeError):
            process.send_interrupt()

        native = MagicMock()
        with patch.object(_interactive, "NativeProcess", return_value=native):
            process.start()
        native.start.assert_called_once()
        with self.assertRaises(RuntimeError):
            process.start()
        native.poll.return_value = None
        self.assertIsNone(process.poll())
        process._finalized = True

    def test_wait_timeout_interrupt_and_abnormal_exit(self) -> None:
        process = _interactive.InteractiveProcess(["unused"], auto_run=False)
        process._proc = MagicMock()
        process._proc.wait.side_effect = TimeoutError()
        with patch.object(process, "kill"), self.assertRaises(TimeoutError):
            process.wait(timeout=0.01)
        self.assertEqual(process.exit_reason, "timeout")

        process._finalized = False
        process._proc.wait.side_effect = None
        process._proc.wait.return_value = 130
        with self.assertRaises(KeyboardInterrupt):
            process.wait()

        process._finalized = False
        process._proc.wait.return_value = 1
        with self.assertRaises(ProcessAbnormalExit):
            process.wait(raise_on_abnormal_exit=True)
        process._finalized = True

    def test_terminate_kill_interrupt_close_and_wait_escalation(self) -> None:
        isolated = _interactive.InteractiveProcess(
            ["unused"], mode=InteractiveMode.CONSOLE_ISOLATED, auto_run=False
        )
        isolated._proc = MagicMock()
        isolated._proc.pid = 123
        isolated._proc.poll.side_effect = [None, None, None]
        with patch.object(isolated, "_wait_for_exit"):
            isolated.terminate()
        isolated._proc.terminate_group.assert_called_once()

        isolated._finalized = False
        with patch.object(isolated, "_wait_for_exit"):
            isolated.kill()
        isolated._proc.kill_group.assert_called_once()

        isolated._finalized = False
        isolated._proc.poll.side_effect = None
        isolated._proc.poll.return_value = None
        with patch.object(_interactive.sys, "platform", "linux"), patch.object(
            _interactive.os, "killpg", create=True
        ) as killpg:
            isolated.send_interrupt()
        killpg.assert_called_once()
        self.assertTrue(isolated.interrupted_by_caller)

        shared = _interactive.InteractiveProcess(["unused"], auto_run=False)
        shared._proc = MagicMock()
        shared._proc.poll.return_value = None
        with patch.object(shared, "_wait_for_exit"):
            shared.terminate()
        shared._proc.terminate.assert_called_once()
        shared._finalized = False
        with patch.object(shared, "_wait_for_exit"):
            shared.kill()
        shared._proc.kill.assert_called_once()
        shared._finalized = False
        shared.send_interrupt()
        shared._proc.send_interrupt.assert_called_once()

        shared._finalized = False
        shared._proc.poll.return_value = 0
        shared.close()
        self.assertEqual(shared.exit_reason, "interrupt")
        shared.close()

        escalating = _interactive.InteractiveProcess(["unused"], auto_run=False)
        escalating._proc = MagicMock()
        escalating._proc.wait.side_effect = [TimeoutError(), 0]
        escalating._wait_for_exit()
        escalating._proc.kill.assert_called_once()
        escalating._finalized = True


if __name__ == "__main__":
    unittest.main()
