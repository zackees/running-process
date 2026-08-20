from __future__ import annotations

import io
import re
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from running_process import expect, priority
from running_process.exit_status import ProcessAbnormalExit, classify_exit_status
from running_process.pty import _command
from running_process.pty._types import InteractiveMode
from running_process.running_process import _helpers, _iter
from running_process.running_process._types import EOS


class RecordingProcess:
    def __init__(self) -> None:
        self.calls: list[tuple[str, object | None]] = []

    def write(self, value: object) -> None:
        self.calls.append(("write", value))

    def terminate(self) -> None:
        self.calls.append(("terminate", None))

    def kill(self) -> None:
        self.calls.append(("kill", None))

    def send_interrupt(self) -> None:
        self.calls.append(("interrupt", None))


class TestCommandEdges(unittest.TestCase):
    def test_windows_and_posix_command_shapes(self) -> None:
        self.assertEqual(_command._windows_pty_command("echo hi", True), ["cmd", "/C", "echo hi"])
        self.assertEqual(
            _command._windows_pty_command(["echo", "hello world"], True),
            ["cmd", "/C", 'echo "hello world"'],
        )
        self.assertEqual(_command._windows_pty_command("tool", False), ["tool"])
        argv = ["tool", "arg"]
        self.assertIs(_command._windows_pty_command(argv, False), argv)

        self.assertEqual(_command._posix_pty_command("echo hi", True), ["sh", "-lc", "echo hi"])
        self.assertEqual(
            _command._posix_pty_command(["echo", "hello world"], True),
            ["sh", "-lc", "echo 'hello world'"],
        )
        self.assertEqual(_command._posix_pty_command("tool", False), ["tool"])
        wrapped = _command._posix_pty_command(["tool"], False, 7)
        self.assertEqual(wrapped[0], _command.sys.executable)
        self.assertEqual(wrapped[3], "7")

        with mock.patch.object(_command.sys, "platform", "win32"):
            self.assertEqual(_command._pty_command("echo hi", True), ["cmd", "/C", "echo hi"])
        with mock.patch.object(_command.sys, "platform", "linux"):
            self.assertEqual(_command._pty_command("echo hi", True), ["sh", "-lc", "echo hi"])

    def test_normalization_nice_and_interactive_modes(self) -> None:
        argv = ["echo", "hi"]
        self.assertEqual(_command._normalize_command(argv, None), (argv, False))
        self.assertEqual(_command._normalize_command(argv, True), (argv, True))
        self.assertEqual(_command._normalize_command("echo hi", True), ("echo hi", True))
        self.assertEqual(
            _command._normalize_command('echo "hi there"', False),
            (["echo", "hi there"], False),
        )
        self.assertEqual(
            _command._normalize_command("echo hi | cat", None),
            ("echo hi | cat", True),
        )
        self.assertEqual(_command._normalize_command("echo hi", None), (["echo", "hi"], False))
        self.assertEqual(_command._strip_wrapping_quotes("'quoted'"), "quoted")
        self.assertEqual(_command._strip_wrapping_quotes("plain"), "plain")

        with mock.patch.object(
            _command,
            "native_apply_process_nice",
            side_effect=RuntimeError("gone"),
        ):
            _command._apply_process_nice(1, 2)
        _command._apply_process_nice(None, 2)
        _command._apply_process_nice(1, None)

        pseudo = _command.interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
        isolated = _command.interactive_launch_spec(InteractiveMode.CONSOLE_ISOLATED)
        shared = _command.interactive_launch_spec(InteractiveMode.CONSOLE_SHARED)
        self.assertTrue(pseudo.uses_pty)
        self.assertEqual(isolated.ctrl_c_owner, "parent")
        self.assertEqual(shared.ctrl_c_owner, "shared")


class TestHelperEdges(unittest.TestCase):
    def test_safe_console_write_uses_binary_fallback(self) -> None:
        class Buffer:
            def __init__(self) -> None:
                self.value = bytearray()

            def write(self, value: bytes) -> None:
                self.value.extend(value)

        class Stream:
            encoding = "ascii"

            def __init__(self) -> None:
                self.buffer = Buffer()
                self.flushed = False

            def write(self, _value: str) -> None:
                raise UnicodeEncodeError("ascii", "é", 0, 1, "ordinal")

            def flush(self) -> None:
                self.flushed = True

        stream = Stream()
        _helpers._safe_console_write(stream, "é")  # type: ignore[arg-type]
        self.assertEqual(bytes(stream.buffer.value), b"?\n")
        self.assertTrue(stream.flushed)

        text = io.StringIO()
        _helpers._safe_console_write(text, b"hello")
        self.assertEqual(text.getvalue(), "hello\n")

    def test_stdin_echo_timestamp_expect_and_shebang_validation(self) -> None:
        self.assertEqual(_helpers._stdin_mode(None, True), "piped")
        self.assertEqual(_helpers._stdin_mode(None, False), "inherit")
        with self.assertRaises(ValueError):
            _helpers._stdin_mode(object(), False)
        with self.assertRaises(TypeError):
            _helpers._validate_echo_flag("yes")  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            _helpers._validate_echo_timestamps("local")
        for stream in ("stdout", "stderr", "combined"):
            self.assertEqual(_helpers._validate_expect_stream(stream), stream)
        with self.assertRaises(ValueError):
            _helpers._validate_expect_stream("both")
        self.assertEqual(_helpers._expect_pattern_spec("text"), ("text", False))
        self.assertEqual(_helpers._expect_pattern_spec(re.compile("x+")), ("x+", True))
        with self.assertRaises(TypeError):
            _helpers._expect_pattern_spec(7)  # type: ignore[arg-type]

        lines: list[str] = []
        with mock.patch.object(_helpers.time, "time", return_value=12.5):
            callback = _helpers._make_timestamped_callback(lines.append, "relative", 10.0)
            callback("ready")
        self.assertEqual(lines, ["[2.50] ready"])
        absolute = _helpers._make_timestamped_callback(lines.append, "absolute", 0)
        absolute("done")
        self.assertRegex(lines[-1], r"^\[\d\d:\d\d:\d\d\.\d{3}\] done$")

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            invalid = root / "invalid"
            invalid.write_text("echo hi\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                _helpers._parse_shebang_command(invalid)
            empty = root / "empty"
            empty.write_text("#!\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                _helpers._parse_shebang_command(empty)
            env = root / "env"
            env.write_text("\ufeff#!/usr/bin/env -S python -u\n", encoding="utf-8")
            self.assertEqual(_helpers._parse_shebang_command(env), ["python", "-u"])
            env_missing = root / "env-missing"
            env_missing.write_text("#!/usr/bin/env\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                _helpers._parse_shebang_command(env_missing)


class TestExpectPriorityAndExitEdges(unittest.TestCase):
    def test_expect_actions_and_searches(self) -> None:
        self.assertIsNone(expect.search_expect_pattern("abc", "z"))
        literal = expect.search_expect_pattern("abc", "b")
        self.assertEqual(literal.span, (1, 2))
        self.assertIsNone(expect.search_expect_pattern("abc", re.compile("z")))
        regex = expect.search_expect_pattern("abc123", re.compile(r"([a-z]+)(\d+)"))
        self.assertEqual(regex.groups, ("abc", "123"))

        process = RecordingProcess()
        match = expect.ExpectMatch("", "", (0, 0), ())
        for action in (None, b"bytes", "terminate", "kill", "interrupt", "text"):
            expect.apply_expect_action(process, action, match)
        self.assertEqual(
            process.calls,
            [
                ("write", b"bytes"),
                ("terminate", None),
                ("kill", None),
                ("interrupt", None),
                ("write", "text"),
            ],
        )
        without_interrupt = SimpleNamespace(calls=[])
        without_interrupt.terminate = lambda: without_interrupt.calls.append("terminate")
        expect.apply_expect_action(without_interrupt, "interrupt", match)
        self.assertEqual(without_interrupt.calls, ["terminate"])
        self.assertEqual(expect.ensure_text(b"caf\xc3\xa9"), "café")

    def test_priority_and_exit_status_cover_all_classifications(self) -> None:
        self.assertIsNone(priority.normalize_nice(None))
        self.assertEqual(priority.normalize_nice(3), 3)
        self.assertEqual(priority.normalize_nice(priority.CpuPriority.MINIMAL), 15)
        self.assertEqual(priority.normalize_nice(priority.CpuPriority.LOW), 5)
        self.assertEqual(priority.normalize_nice(priority.CpuPriority.NORMAL), 0)
        self.assertEqual(priority.normalize_nice(priority.CpuPriority.HIGH), -5)
        with self.assertRaises(TypeError):
            priority.normalize_nice("high")  # type: ignore[arg-type]

        interrupted = classify_exit_status(130, {130}, "linux")
        self.assertIn("interrupted", interrupted.summary)
        signal_status = classify_exit_status(-15, set(), "linux")
        self.assertIn("SIGTERM", signal_status.summary)
        unknown_signal = classify_exit_status(-999, set(), "linux")
        self.assertIn("signal 999", unknown_signal.summary)
        killed = classify_exit_status(-9, set(), "linux")
        self.assertTrue(killed.possible_oom)
        windows_oom = classify_exit_status(3221225495, set(), "win32")
        self.assertIn("out-of-memory", windows_oom.summary)
        abnormal = classify_exit_status(2, set(), "linux")
        self.assertIn("abnormally", abnormal.summary)
        normal = classify_exit_status(0, set(), "linux")
        self.assertIn("normally", normal.summary)
        self.assertIs(ProcessAbnormalExit(abnormal).status, abnormal)


class FakeNativeProcess:
    def __init__(self, responses: list[tuple[str, str | None, str | None]]) -> None:
        self.responses = responses
        self.wait_calls: list[float | None] = []

    def take_combined_line(self, _timeout: float | None):
        return self.responses.pop(0)

    def wait(self, timeout: float | None = None) -> int:
        self.wait_calls.append(timeout)
        return 7


class TestOutputIteratorEdges(unittest.TestCase):
    def process(self, responses):
        return SimpleNamespace(
            _pty_process=None,
            capture=True,
            _proc=FakeNativeProcess(responses),
            _end_time=None,
            returncode=None,
            poll=lambda: None,
            _format=lambda line: line.upper(),
        )

    def test_iterator_rejects_invalid_backends_and_covers_lines_timeout_and_eos(self) -> None:
        invalid = self.process([])
        invalid._pty_process = object()
        with self.assertRaises(NotImplementedError):
            next(_iter._RunningProcessOutputIterator(invalid, None))
        invalid._pty_process = None
        invalid.capture = False
        with self.assertRaises(NotImplementedError):
            next(_iter._RunningProcessOutputIterator(invalid, None))

        timed = self.process([("timeout", None, None)])
        with self.assertRaises(TimeoutError):
            next(_iter._RunningProcessOutputIterator(timed, 1))

        process = self.process(
            [("line", "stdout", "out"), ("line", "stderr", "err"), ("eof", None, None)]
        )
        iterator = _iter._RunningProcessOutputIterator(process, 1)
        self.assertIs(iter(iterator), iterator)
        self.assertEqual(next(iterator).stdout, "OUT")
        self.assertEqual(next(iterator).stderr, "ERR")
        with mock.patch.object(_iter.RunningProcessManagerSingleton, "unregister"):
            drained = next(iterator)
        self.assertIs(drained.stdout, EOS)
        self.assertEqual(drained.exit_code, 7)
        with self.assertRaises(StopIteration):
            next(iterator)

        delayed = self.process([("eof", None, None)])
        delayed._proc.wait = mock.Mock(side_effect=[TimeoutError, 7])
        iterator = _iter._RunningProcessOutputIterator(delayed, None)
        first = next(iterator)
        self.assertIsNone(first.exit_code)
        with mock.patch.object(_iter.RunningProcessManagerSingleton, "unregister"):
            final = next(iterator)
        self.assertEqual(final.exit_code, 7)


if __name__ == "__main__":
    unittest.main()
