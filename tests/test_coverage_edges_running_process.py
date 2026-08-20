from __future__ import annotations

import os
import unittest
from unittest.mock import MagicMock, patch

from running_process.compat import PIPE
from running_process.running_process import _core
from running_process.running_process._core import EOS, RunningProcess


def bare(*, pty: bool = False, capture: bool = True) -> RunningProcess:
    process = RunningProcess.__new__(RunningProcess)
    process.command = ["fixture", "arg"]
    process.shell = False
    process.capture = capture
    process.text = True
    process.encoding = "utf-8"
    process.errors = "replace"
    process.on_timeout = None
    process._output_formatter = MagicMock()
    process._output_formatter.transform.side_effect = lambda value: value
    process._on_complete = None
    process._start_time = None
    process._end_time = None
    process._exit_status = None
    process._process_watches = []
    process._process_watch_by_label = {}
    process._native_process_kwargs = {}
    process._proc = MagicMock()
    process._proc.pid = os.getpid()
    process._pty_process = MagicMock() if pty else None
    if process._pty_process is not None:
        process._pty_process.pid = os.getpid()
    return process


class RunningProcessValidationCoverageTest(unittest.TestCase):
    def test_constructor_rejects_every_incompatible_shape(self) -> None:
        watch = MagicMock(label="duplicate")
        cases = [
            dict(process_watches=[watch, watch]),
            dict(stderr=object()),
            dict(capture=False, stderr=PIPE),
            dict(use_pty=True, process_watches=[watch]),
            dict(use_pty=True, address_space_limit_bytes=1),
            dict(use_pty=True, stdin=object()),
            dict(use_pty=True, stderr=PIPE),
            dict(relay_terminal_input=True),
            dict(arm_idle_timeout_on_submit=True),
        ]
        for kwargs in cases:
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                RunningProcess(["unused"], auto_run=False, **kwargs)

    def test_native_constructor_errors_preserve_observation_context(self) -> None:
        watch = MagicMock(label="watch")
        watch._native.return_value = {"label": "watch"}
        with patch.object(_core, "NativeProcess", side_effect=RuntimeError("unavailable")):
            with self.assertRaises(_core.ProcessObservationUnavailableError):
                RunningProcess(
                    ["unused"],
                    auto_run=False,
                    process_watches=[watch],
                )
            with self.assertRaisesRegex(RuntimeError, "unavailable"):
                RunningProcess(["unused"], auto_run=False)

    def test_capabilities_and_prestart_watch_errors(self) -> None:
        raw = {
            "exact_available": True,
            "exact_backend": "trace",
            "reason": "ready",
            "non_invasive_backend": "poll",
            "non_invasive_grade": "snapshot_inferred",
        }
        with patch.object(
            _core.NativeProcess,
            "process_observation_capabilities",
            return_value=raw,
        ):
            capabilities = RunningProcess.process_observation_capabilities()
        self.assertTrue(capabilities.exact_available)
        self.assertEqual(capabilities.exact_backend, "trace")

        process = bare()
        process._start_time = 1.0
        with self.assertRaises(RuntimeError):
            process.add_process_watch(MagicMock(label="new"))
        process._start_time = None
        process._pty_process = MagicMock()
        with self.assertRaises(ValueError):
            process.add_process_watch(MagicMock(label="new"))
        process._pty_process = None
        process._process_watch_by_label = {"duplicate": MagicMock()}
        with self.assertRaises(ValueError):
            process.add_process_watch(MagicMock(label="duplicate"))


class RunningProcessStreamCoverageTest(unittest.TestCase):
    def test_pipe_line_statuses_drains_and_pending_state(self) -> None:
        process = bare()
        process._proc.take_combined_line.side_effect = [
            ("line", "stdout", "line"),
            ("timeout", "stdout", None),
            ("eof", "stdout", None),
        ]
        self.assertEqual(process.get_next_line(), "line")
        with self.assertRaises(TimeoutError):
            process.get_next_line()
        self.assertIs(process.get_next_line(), EOS)

        process._proc.take_stream_line.side_effect = [
            ("line", "out"),
            ("timeout", None),
            ("eof", None),
            ("line", "err"),
            ("timeout", None),
            ("eof", None),
        ]
        self.assertEqual(process.get_next_stdout_line(), "out")
        with self.assertRaises(TimeoutError):
            process.get_next_stdout_line()
        self.assertIs(process.get_next_stdout_line(), EOS)
        self.assertEqual(process.get_next_stderr_line(), "err")
        with self.assertRaises(TimeoutError):
            process.get_next_stderr_line()
        self.assertIs(process.get_next_stderr_line(), EOS)

        process._proc.take_combined_line.return_value = ("timeout", "", None)
        process._proc.take_combined_line.side_effect = None
        self.assertIsNone(process.get_next_line_non_blocking())
        process._proc.drain_stream.side_effect = [["a"], ["b"]]
        process._proc.drain_combined.return_value = [("stdout", "a"), ("stderr", "b")]
        self.assertEqual(process.drain_stdout(), ["a"])
        self.assertEqual(process.drain_stderr(), ["b"])
        self.assertEqual(process.drain_combined(), [("stdout", "a"), ("stderr", "b")])
        process._proc.has_pending_combined.return_value = True
        process._proc.has_pending_stream.side_effect = [True, False]
        self.assertTrue(process.has_pending_output())
        self.assertTrue(process.has_pending_stdout())
        self.assertFalse(process.has_pending_stderr())

    def test_pty_line_drains_pending_and_idle_property_branches(self) -> None:
        process = bare(pty=True, capture=False)
        with self.assertRaises(NotImplementedError):
            process.get_next_line()
        with self.assertRaises(NotImplementedError):
            process.get_next_stdout_line()
        with self.assertRaises(NotImplementedError):
            process.get_next_stderr_line()
        self.assertEqual(process.drain_stdout(), [])
        self.assertEqual(process.drain_stderr(), [])
        self.assertEqual(process.drain_combined(), [])
        self.assertFalse(process.has_pending_output())
        self.assertFalse(process.has_pending_stdout())
        self.assertFalse(process.has_pending_stderr())

        process.capture = True
        process._pty_process.read.side_effect = ["pty-line", EOFError()]
        self.assertEqual(process.get_next_line(), "pty-line")
        self.assertIs(process.get_next_line(), EOS)
        process._pty_process.poll.side_effect = [0, None]
        self.assertIs(process.get_next_stderr_line(), EOS)
        with self.assertRaises(TimeoutError):
            process.get_next_stderr_line()
        process._pty_process.drain.return_value = ["one", "two"]
        self.assertEqual(process.drain_stdout(), ["one", "two"])
        self.assertEqual(
            process.drain_combined(),
            [("stdout", "one"), ("stdout", "two")],
        )
        process._pty_process.available.return_value = True
        self.assertTrue(process.has_pending_output())
        self.assertTrue(process.has_pending_stdout())

        pipe = bare()
        with self.assertRaises(AttributeError):
            _ = pipe.idle_timeout_enabled
        with self.assertRaises(AttributeError):
            pipe.idle_timeout_enabled = True
        process._pty_process.idle_timeout_enabled = False
        self.assertFalse(process.idle_timeout_enabled)
        process.idle_timeout_enabled = True
        self.assertTrue(process._pty_process.idle_timeout_enabled)
        process._pty_process.poll.side_effect = None
        process._pty_process.poll.return_value = 0


class RunningProcessLifecycleCoverageTest(unittest.TestCase):
    def test_observation_watch_cursor_and_process_info_empty_branches(self) -> None:
        process = bare()
        process._proc = None
        self.assertIsNone(process.process_observation)
        self.assertEqual(process.watch_matches, ())
        with self.assertRaises(RuntimeError):
            process.process_watch_cursor()

        process._proc = MagicMock()
        process._proc.process_observation.side_effect = [None, {
            "backend": "poll",
            "observation_grade": "snapshot_inferred",
            "fallback_reason": "fallback",
        }]
        self.assertIsNone(process.process_observation)
        self.assertEqual(process.process_observation.backend, "poll")
        with self.assertRaises(RuntimeError):
            process.process_watch_cursor()

        process._proc.pid = None
        info = process._create_process_info()
        self.assertEqual(info.pid, 0)
        self.assertEqual(info.duration, 0.0)
        self.assertIn("fixture", process.get_command_str())
        process.command = "already rendered"
        self.assertEqual(process.get_command_str(), "already rendered")

    def test_start_timeout_signal_and_close_cover_both_backends(self) -> None:
        with patch.object(_core.RunningProcessManagerSingleton, "register"), patch.object(
            _core.RunningProcessManagerSingleton, "unregister"
        ):
            pipe = bare()
            pipe.start()
            pipe._proc.start.assert_called_once()
            self.assertIs(pipe.proc, pipe._proc)
            pipe.kill()
            pipe.terminate()
            pipe.send_interrupt()
            pipe.close()

            pty = bare(pty=True)
            pty.start()
            pty.kill()
            pty.terminate()
            pty.send_interrupt()
            pty._pty_process.poll.return_value = 0
            pty.close()
            pty._pty_process.poll.return_value = None
            pty.close()

        timed = bare()
        callback = MagicMock()
        timed.on_timeout = callback
        with patch.object(timed, "kill"), self.assertRaises(TimeoutError):
            timed._handle_timeout(0.25)
        callback.assert_called_once()

    def test_status_duration_capture_discard_write_and_expect_branches(self) -> None:
        pipe = bare()
        pipe._proc.returncode = None
        self.assertIsNone(pipe.exit_status)
        self.assertIsNone(pipe.duration)
        pipe._start_time = 2.0
        pipe._end_time = 5.5
        self.assertEqual(pipe.duration, 3.5)
        pipe._proc.returncode = 0
        self.assertIsNotNone(pipe.exit_status)
        self.assertIs(pipe.exit_status, pipe.exit_status)

        pipe._proc.captured_stdout.return_value = ["a", "b"]
        pipe._proc.captured_stderr.return_value = ["e"]
        pipe._proc.captured_combined.return_value = [("stdout", "a"), ("stderr", "e")]
        self.assertEqual(pipe.stdout, "a\nb")
        self.assertEqual(pipe.stderr, "e")
        self.assertEqual(pipe.combined_output, "a\ne")
        pipe.text = False
        pipe._proc.captured_stdout.return_value = [b"a", b"b"]
        self.assertEqual(pipe.stdout, b"a\nb")
        pipe._proc.clear_captured_combined.return_value = 3
        pipe._proc.clear_captured_stream.return_value = 2
        pipe._proc.captured_combined_bytes.return_value = 4
        pipe._proc.captured_stream_bytes.return_value = 5
        self.assertEqual(pipe.discard_captured_output(), 3)
        self.assertEqual(pipe.discard_captured_output("stdout"), 2)
        self.assertEqual(pipe.captured_output_bytes(), 4)
        self.assertEqual(pipe.captured_output_bytes("stderr"), 5)
        pipe.write("text")
        pipe.write(b"bytes")
        self.assertEqual(pipe._proc.write_stdin.call_count, 2)

        pipe._proc.expect.side_effect = [
            ("timeout", "", None, None, None, []),
            ("eof", "", None, None, None, []),
        ]
        with self.assertRaises(TimeoutError):
            pipe.expect("missing")
        with self.assertRaises(EOFError):
            pipe.expect("missing")

        pty = bare(pty=True)
        pty._pty_process.output = b"output"
        pty._pty_process.output_bytes = 6
        pty._pty_process.discard_output.return_value = 6
        self.assertEqual(pty.stdout, b"output")
        self.assertEqual(pty.stderr, b"")
        self.assertEqual(pty.discard_captured_output("stderr"), 0)
        self.assertEqual(pty.discard_captured_output(), 6)
        self.assertEqual(pty.captured_output_bytes("stderr"), 0)
        self.assertEqual(pty.captured_output_bytes(), 6)
        pty.write("input", submit=True)
        with self.assertRaises(ValueError):
            pty.expect("x", stream="stderr")


if __name__ == "__main__":
    unittest.main()
