"""Sync/async lifecycle contract tests for the remaining #875 parity rows.

The operations here already existed on both surfaces before #875. What did not
exist was a test that pins them *against each other* -- and citing whatever
pre-existing test happens to touch a method is a guess, not evidence. So each
family gets one sync contract and one async contract written against the same
expectation, and the manifest cites those.

Grouped by family rather than one test per member: `start`/`wait`/`pid` are not
independently observable, and pretending otherwise would mean six tests that
each assert a third of the same thing.
"""

from __future__ import annotations

import os
import sys
import unittest

from running_process.asyncio import (
    AsyncPseudoTerminalProcess,
    AsyncRunningProcess,
)
from running_process.pty import PseudoTerminalProcess, Pty
from running_process.running_process import RunningProcess

# PTY mode always uses raw bytes; the warning about that is expected here.
os.environ.setdefault("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")

ECHO = "print('lifecycle')"
SLEEPER = "import time; time.sleep(60)"
LIVE_ECHO = (
    "import sys, time; print('lifecycle'); sys.stdout.flush(); time.sleep(30)"
)
STDIN_ECHO = "import sys; sys.stdout.write(sys.stdin.read())"


class TestPipeLifecycleAsync(unittest.IsolatedAsyncioTestCase):
    async def test_start_pid_wait_report_a_consistent_lifecycle(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", ECHO])
        await process.start()
        pid = await process.pid()
        self.assertGreater(pid, 0)
        self.assertEqual(await process.wait(), 0)

    async def test_start_twice_is_rejected(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", ECHO])
        await process.start()
        with self.assertRaises(RuntimeError):
            await process.start()
        await process.wait()

    async def test_pid_before_start_is_rejected(self) -> None:
        with self.assertRaises(RuntimeError):
            await AsyncRunningProcess(sys.executable, ["-c", ECHO]).pid()

    async def test_kill_ends_a_running_child(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", SLEEPER])
        await process.start()
        await process.kill()
        self.assertNotEqual(await process.wait(), 0)

    async def test_stdin_write_then_close_delivers_eof(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", STDIN_ECHO])
        await process.start()
        await process.write_stdin(b"piped-input")
        await process.close_stdin()
        code, stdout, _stderr = await process.output()
        self.assertEqual(code, 0)
        self.assertIn(b"piped-input", stdout)

    async def test_output_returns_both_streams_and_the_exit_code(self) -> None:
        script = "import sys; print('out'); print('err', file=sys.stderr)"
        process = AsyncRunningProcess(sys.executable, ["-c", script])
        await process.start()
        code, stdout, stderr = await process.output()
        self.assertEqual(code, 0)
        self.assertIn(b"out", stdout)
        self.assertIn(b"err", stderr)

    async def test_output_bounded_caps_what_it_retains(self) -> None:
        script = "print('x' * 10000)"
        process = AsyncRunningProcess(sys.executable, ["-c", script])
        await process.start()
        with self.assertRaises(RuntimeError):
            await process.output_bounded(16)


class TestPipeLifecycleSync(unittest.TestCase):
    """The sync half of the rows above."""

    def test_start_pid_wait_report_a_consistent_lifecycle(self) -> None:
        process = RunningProcess([sys.executable, "-c", ECHO], auto_run=False)
        self.assertFalse(process.is_started)
        process.start()
        self.assertTrue(process.is_started)
        self.assertGreater(process.pid or 0, 0)
        self.assertEqual(process.wait(), 0)
        self.assertTrue(process.finished)
        self.assertFalse(process.is_running())

    def test_kill_ends_a_running_child(self) -> None:
        process = RunningProcess([sys.executable, "-c", SLEEPER], auto_run=False)
        process.start()
        self.assertTrue(process.is_running())
        process.kill()
        self.assertNotEqual(process.wait(), 0)

    @unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
    def test_write_then_submit_delivers_input_to_the_child(self) -> None:
        """`write`/`submit` on RunningProcess, via its PTY compatibility mode.

        The pipe backend routes writes through the native stdin pipe, which the
        constructor only opens for some stdin configurations; PTY mode is the
        path where `write`/`submit` are the documented way to talk to a child,
        and `capture=True` is what makes the reply observable.
        """
        process = RunningProcess(
            [sys.executable, "-c", "print(input())"],
            auto_run=False,
            use_pty=True,
            capture=True,
        )
        process.start()
        try:
            process.write("piped-input")
            process.submit()
            match = process.expect("piped-input", timeout=20)
            self.assertEqual(match.matched, "piped-input")
        finally:
            process.close()

    def test_captured_output_carries_both_streams(self) -> None:
        script = "import sys; print('out'); print('err', file=sys.stderr)"
        process = RunningProcess([sys.executable, "-c", script], auto_run=False)
        process.start()
        process.wait()
        combined = str(process.combined_output)
        self.assertIn("out", combined)
        self.assertIn("err", combined)

    def test_captured_output_bytes_and_discard_release_history(self) -> None:
        process = RunningProcess([sys.executable, "-c", "print('x' * 500)"], auto_run=False)
        process.start()
        process.wait()
        self.assertGreater(process.captured_output_bytes(), 0)
        process.discard_captured_output()
        self.assertEqual(process.captured_output_bytes(), 0)

    def test_the_compat_alias_reports_what_the_real_spelling_does(self) -> None:
        # `is_runninng` is a typo kept for compatibility. It is part of the
        # frozen surface, so it gets a contract like anything else.
        process = RunningProcess([sys.executable, "-c", SLEEPER], auto_run=False)
        process.start()
        try:
            self.assertEqual(process.is_runninng(), process.is_running())
        finally:
            process.kill()
            process.wait()

    def test_metadata_is_populated_after_a_run(self) -> None:
        process = RunningProcess([sys.executable, "-c", ECHO], auto_run=False)
        self.assertIsNone(process.start_time)
        process.start()
        process.wait()
        self.assertIsNotNone(process.start_time)
        self.assertIsNotNone(process.end_time)
        self.assertIsNotNone(process.duration)
        self.assertIsNotNone(process.exit_status)
        self.assertIn(sys.executable, process.get_command_str())
        self.assertIsNotNone(process.proc)

    def test_streaming_reads_and_availability_agree(self) -> None:
        script = "import sys; print('a'); print('b'); sys.stdout.flush()"
        process = RunningProcess([sys.executable, "-c", script], auto_run=False)
        process.start()
        process.wait()
        # The drain family consumes; `has_pending_*` is how a caller knows
        # whether there is anything left to consume.
        self.assertTrue(process.has_pending_output() or process.has_pending_stdout())
        drained = process.drain_stdout() + process.drain_combined()
        self.assertTrue(drained)
        self.assertFalse(process.has_pending_output())
        self.assertEqual(process.drain_stderr(), [])


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestPtyLifecycleAsync(unittest.IsolatedAsyncioTestCase):
    async def test_start_pid_read_wait_close_form_a_lifecycle(self) -> None:
        # See the note in tests/test_async_expect_and_helpers.py: the async PTY
        # reads on demand, so the child must still be alive to be read.
        process = AsyncPseudoTerminalProcess([sys.executable, "-c", LIVE_ECHO])
        await process.start()
        try:
            self.assertIsNotNone(await process.pid())
            match = await process.expect("lifecycle", timeout=20)
            self.assertEqual(match.matched, "lifecycle")
        finally:
            await process.kill()
            await process.close()

    async def test_resize_is_accepted_on_a_running_pty(self) -> None:
        process = AsyncPseudoTerminalProcess([sys.executable, "-c", SLEEPER])
        await process.start()
        try:
            await process.resize(40, 100)
        finally:
            await process.kill()
            await process.close()

    async def test_write_reaches_the_child(self) -> None:
        process = AsyncPseudoTerminalProcess(
            [sys.executable, "-c", "print(input())"]
        )
        await process.start()
        try:
            await process.write(b"pty-input\r\n", True)
            match = await process.expect("pty-input", timeout=20)
            self.assertEqual(match.matched, "pty-input")
        finally:
            await process.kill()
            await process.close()

    async def test_terminate_ends_the_child_and_a_following_kill_is_safe(
        self,
    ) -> None:
        """`terminate` reaps; a belt-and-braces `kill` after it must be safe.

        Whether that second call is a no-op or raises "not running" differs by
        platform -- POSIX accepts it, ConPTY rejects it -- and neither is
        wrong. The contract is that teardown code calling both does not hang
        and leaves the child ended, so that is what is asserted rather than
        whichever spelling the host happens to use.
        """
        process = AsyncPseudoTerminalProcess([sys.executable, "-c", SLEEPER])
        await process.start()
        await process.terminate()
        try:
            await process.kill()
        except RuntimeError:
            pass
        await process.close()
        self.assertIsNotNone(await process.wait(5.0))


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestPtyLifecycleSync(unittest.TestCase):
    """The sync half of the PTY rows above."""

    def test_start_pid_read_wait_close_form_a_lifecycle(self) -> None:
        process = PseudoTerminalProcess([sys.executable, "-c", ECHO])
        try:
            self.assertTrue(Pty.is_available())
            self.assertIsNotNone(process.pid)
            match = process.expect("lifecycle", timeout=20)
            self.assertEqual(match.matched, "lifecycle")
            self.assertIsNotNone(process.wait())
            self.assertFalse(process.is_running)
            self.assertIsNotNone(process.exit_status)
            self.assertGreater(process.output_bytes, 0)
            self.assertIn("lifecycle", process.output_text)
        finally:
            process.close()

    def test_resize_and_write_are_accepted_on_a_running_pty(self) -> None:
        process = PseudoTerminalProcess([sys.executable, "-c", "print(input())"])
        try:
            process.resize(40, 100)
            process.write("pty-input")
            process.submit()
            match = process.expect("pty-input", timeout=20)
            self.assertEqual(match.matched, "pty-input")
        finally:
            process.close()

    def test_reads_and_discard_release_history(self) -> None:
        process = PseudoTerminalProcess([sys.executable, "-c", ECHO])
        try:
            process.expect("lifecycle", timeout=20)
            # `expect` has consumed the stream, so a further read has nothing
            # to give and reports that by raising -- which is the contract.
            with self.assertRaises((TimeoutError, EOFError)):
                process.read_text(timeout=0.2)
            # A drained-and-closed stream raises rather than returning empty
            # on POSIX, and returns empty on ConPTY. Both mean "nothing left".
            for drain in (process.drain, process.drain_echo):
                try:
                    self.assertIsInstance(drain(), (bytes, bytearray, str, list))
                except EOFError:
                    pass
            self.assertGreater(process.output_bytes, 0)
            process.discard_output()
            self.assertEqual(process.output_bytes, 0)
            try:
                self.assertIsNone(process.read_non_blocking())
            except EOFError:
                pass
        finally:
            process.close()

    def test_terminate_and_kill_are_idempotent_after_exit(self) -> None:
        process = PseudoTerminalProcess([sys.executable, "-c", ECHO])
        process.wait()
        process.terminate()
        process.kill()
        process.close()
        self.assertIsNotNone(process.poll())


if __name__ == "__main__":
    unittest.main()
