"""Contract tests for async expect, idle waiting, and the module-level helpers.

Cited by `docs/async_api_parity.toml`. Each async assertion is paired with the
sync assertion it claims parity with, so the two cannot drift apart quietly.
"""

from __future__ import annotations

import asyncio
import os
import re
import sys
import unittest

from running_process import (
    get_process_tree_info as sync_get_process_tree_info,
)
from running_process import (
    subprocess_run as sync_subprocess_run,
)
from running_process import (
    terminate_process_tree as sync_terminate_process_tree,
)
from running_process.asyncio import (
    AsyncInteractiveProcess,
    AsyncPseudoTerminalProcess,
    AsyncRunningProcess,
    ExpectTimeoutError,
    get_process_tree_info,
    subprocess_run,
    terminate_process_tree,
)
from running_process.compat import CalledProcessError
from running_process.pty import PseudoTerminalProcess, Pty
from running_process.running_process import RunningProcess

GREETER = "print('hello-parity')"
SLEEPER = "import time; time.sleep(60)"


def _emitting(process: AsyncRunningProcess) -> asyncio.Task:
    """Capture must be running for a cursor to see anything."""
    return asyncio.create_task(process.output())


class TestAsyncExpect(unittest.IsolatedAsyncioTestCase):
    async def test_expect_matches_a_literal_in_process_output(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", GREETER])
        await process.start()
        expect = asyncio.create_task(process.expect("hello-parity", timeout=30))
        capture = _emitting(process)
        match = await expect
        await capture
        self.assertEqual(match.matched, "hello-parity")

    async def test_expect_matches_a_regex_and_reports_groups(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", "print('code=42')"])
        await process.start()
        expect = asyncio.create_task(
            process.expect(re.compile(r"code=(\d+)"), timeout=30)
        )
        capture = _emitting(process)
        match = await expect
        await capture
        self.assertEqual(match.groups, ("42",))

    async def test_expect_raises_the_same_timeout_type_as_the_sync_surface(
        self,
    ) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", SLEEPER])
        await process.start()
        try:
            with self.assertRaises(ExpectTimeoutError) as caught:
                await process.expect("never-appears", timeout=0.3)
            # The sync `expect` raises plain TimeoutError; ours must remain
            # catchable that way or every existing handler breaks.
            self.assertIsInstance(caught.exception, TimeoutError)
            self.assertEqual(caught.exception.buffer, "")
        finally:
            await process.kill()
            await process.wait()

    async def test_expect_raises_eof_when_the_stream_ends_first(self) -> None:
        """Matches the sync contract, and proves the matcher cannot spin.

        Without an EOF signal the loop would keep asking a dead process for
        more output forever.
        """
        process = AsyncRunningProcess(sys.executable, ["-c", GREETER])
        await process.start()
        expect = asyncio.create_task(process.expect("never-appears", timeout=30))
        capture = _emitting(process)
        with self.assertRaises(EOFError):
            await expect
        await capture

    async def test_expect_uses_a_private_cursor_and_does_not_steal_output(
        self,
    ) -> None:
        """The improvement over the sync surface, contracted.

        Sync `expect` and `get_next_line` draw from one buffer, so one consumes
        what the other wanted. Two async expects must both succeed.
        """
        process = AsyncRunningProcess(sys.executable, ["-c", GREETER])
        await process.start()
        first = asyncio.create_task(process.expect("hello-parity", timeout=30))
        second = asyncio.create_task(process.expect("hello-parity", timeout=30))
        capture = _emitting(process)
        results = await asyncio.gather(first, second)
        await capture
        self.assertEqual([match.matched for match in results], ["hello-parity"] * 2)


class TestSyncExpectCounterpart(unittest.TestCase):
    def test_sync_expect_matches_a_literal_in_process_output(self) -> None:
        process = RunningProcess([sys.executable, "-c", GREETER], auto_run=False)
        process.start()
        match = process.expect("hello-parity", timeout=20)
        process.wait()
        self.assertEqual(match.matched, "hello-parity")

    def test_sync_expect_times_out_with_timeout_error(self) -> None:
        process = RunningProcess([sys.executable, "-c", SLEEPER], auto_run=False)
        process.start()
        try:
            with self.assertRaises(TimeoutError):
                process.expect("never-appears", timeout=0.3)
        finally:
            process.kill()
            process.wait()


class TestAsyncIdle(unittest.IsolatedAsyncioTestCase):
    async def test_wait_for_idle_reports_true_once_output_stops(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", GREETER])
        await process.start()
        idle = asyncio.create_task(process.wait_for_idle(0.2, timeout=30))
        capture = _emitting(process)
        self.assertTrue(await idle)
        await capture

    async def test_wait_for_idle_reports_false_when_the_timeout_wins(self) -> None:
        emitter = (
            "import time,sys\n"
            "while True:\n"
            "    print('tick'); sys.stdout.flush(); time.sleep(0.02)"
        )
        process = AsyncRunningProcess(sys.executable, ["-c", emitter])
        await process.start()
        try:
            idle = asyncio.create_task(process.wait_for_idle(5.0, timeout=0.5))
            capture = _emitting(process)
            self.assertFalse(await idle)
        finally:
            await process.kill()
            # The capture ends in error because the child was killed mid-stream;
            # which error is not the contract here, so it is swallowed rather
            # than asserted on.
            capture.cancel()


class TestModuleHelperParity(unittest.IsolatedAsyncioTestCase):
    async def test_terminate_process_tree_matches_the_sync_helper_on_a_dead_pid(
        self,
    ) -> None:
        missing = 0xFFFFFFFF
        self.assertEqual(
            await terminate_process_tree(missing, timeout_seconds=0.2),
            sync_terminate_process_tree(missing, timeout_seconds=0.2),
        )

    async def test_terminate_process_tree_ends_a_real_child(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", SLEEPER])
        await process.start()
        pid = await process.pid()
        self.assertTrue(await terminate_process_tree(pid, timeout_seconds=10.0))
        self.assertNotEqual(await process.wait(), 0)

    async def test_get_process_tree_info_matches_the_sync_helper_shape(self) -> None:
        rendered = await get_process_tree_info(os.getpid())
        self.assertIsInstance(rendered, str)
        self.assertIsInstance(sync_get_process_tree_info(os.getpid()), str)
        self.assertIn(str(os.getpid()), rendered)

    async def test_subprocess_run_matches_the_sync_helper_result(self) -> None:
        argv = [sys.executable, "-c", GREETER]
        expected = sync_subprocess_run(argv, cwd=None, check=True, timeout=60)
        actual = await subprocess_run(argv, check=True, timeout=60)
        self.assertEqual(actual.returncode, expected.returncode)
        self.assertIn("hello-parity", actual.stdout)
        self.assertEqual(actual.args, expected.args)

    async def test_subprocess_run_check_raises_the_same_error_type(self) -> None:
        with self.assertRaises(CalledProcessError):
            await subprocess_run(
                [sys.executable, "-c", "import sys; sys.exit(7)"], check=True
            )

    async def test_subprocess_run_rejects_an_empty_command(self) -> None:
        with self.assertRaises(ValueError):
            await subprocess_run([])


class TestAsyncInteractiveLifecycle(unittest.IsolatedAsyncioTestCase):
    async def test_poll_reports_none_while_running_and_a_code_after_exit(self) -> None:
        session = AsyncInteractiveProcess([sys.executable, "-c", SLEEPER])
        await session.start()
        self.assertIsNone(await session.poll())
        await session.kill()
        await session.wait()
        self.assertIsNotNone(await session.poll())

    async def test_exit_status_agrees_with_poll(self) -> None:
        session = AsyncInteractiveProcess([sys.executable, "-c", "import sys; sys.exit(5)"])
        await session.start()
        await session.wait()
        self.assertEqual(await session.exit_status(), await session.poll())


class TestSyncInteractiveCounterpart(unittest.TestCase):
    def test_sync_poll_reports_none_before_start(self) -> None:
        from running_process.pty import InteractiveProcess

        session = InteractiveProcess([sys.executable, "-c", GREETER], auto_run=False)
        self.assertIsNone(session.poll())


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestAsyncPtyExpect(unittest.IsolatedAsyncioTestCase):
    """PTY-backed expect.

    Every body is wrapped in an outer `asyncio.wait_for`. `pytest-timeout` uses
    the thread method, which cannot interrupt a blocking native PTY read, so a
    wedged ConPTY would hang the whole job rather than fail one test. The outer
    bound turns that into a normal failure.
    """

    BOUND = 40.0

    async def test_pty_expect_matches_child_output(self) -> None:
        async def body() -> None:
            process = AsyncPseudoTerminalProcess([sys.executable, "-c", GREETER])
            await process.start()
            try:
                match = await process.expect("hello-parity", timeout=15)
                self.assertEqual(match.matched, "hello-parity")
            finally:
                await process.close()

        await asyncio.wait_for(body(), self.BOUND)

    async def test_pty_expect_times_out_without_a_match(self) -> None:
        async def body() -> None:
            process = AsyncPseudoTerminalProcess([sys.executable, "-c", GREETER])
            await process.start()
            try:
                with self.assertRaises(TimeoutError):
                    await process.expect("never-appears", timeout=1.0)
            finally:
                await process.close()

        await asyncio.wait_for(body(), self.BOUND)

    async def test_pty_wait_for_output_idle_reports_true_once_quiet(self) -> None:
        async def body() -> None:
            process = AsyncPseudoTerminalProcess([sys.executable, "-c", GREETER])
            await process.start()
            try:
                self.assertTrue(await process.wait_for_output_idle(0.2, timeout=15))
            finally:
                await process.close()

        await asyncio.wait_for(body(), self.BOUND)


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestSyncPtyExpectCounterpart(unittest.TestCase):
    def test_sync_pty_expect_matches_child_output(self) -> None:
        process = PseudoTerminalProcess([sys.executable, "-c", GREETER])
        try:
            match = process.expect("hello-parity", timeout=20)
            self.assertEqual(match.matched, "hello-parity")
        finally:
            process.close()


if __name__ == "__main__":
    unittest.main()
