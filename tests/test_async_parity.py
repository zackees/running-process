"""Python sync/async parity contracts for #875.

Each test here is cited by a row in ``docs/async_api_parity.toml``. As on the
Rust side, the point is not that the async method runs -- it is that it agrees
with the sync method it claims parity with.

Nothing in this file may reach for ``asyncio.to_thread`` or an executor. The
awaitables come from the PyO3 extension, and
``tests/test_async_process_bridge.py`` already proves that; these are about
behaviour.
"""

from __future__ import annotations

import sys
import unittest

from running_process.asyncio import (
    AsyncInteractiveProcess,
    AsyncPseudoTerminalProcess,
    AsyncRunningProcess,
    kill_process_tree,
)
from running_process.process_utils import kill_process_tree as sync_kill_process_tree
from running_process.pty import PseudoTerminalProcess, Pty
from running_process.running_process import RunningProcess

SLEEP_SCRIPT = "import time; time.sleep(60)"
EXIT_SCRIPT = "import sys; sys.exit(3)"


def _async_sleeper(*, create_process_group: bool = False) -> AsyncRunningProcess:
    return AsyncRunningProcess(
        sys.executable, ["-c", SLEEP_SCRIPT], create_process_group=create_process_group
    )


def _pty_argv() -> list[str]:
    return [sys.executable, "-c", "print('parity')"]


class TestProcessLifecycleParity(unittest.IsolatedAsyncioTestCase):
    async def test_async_poll_reports_none_while_running_and_a_code_after_exit(
        self,
    ) -> None:
        process = _async_sleeper()
        await process.start()
        self.assertIsNone(await process.poll())
        await process.kill()
        await process.wait()
        self.assertIsNotNone(await process.poll())

    async def test_async_returncode_matches_the_code_the_child_chose(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", EXIT_SCRIPT])
        await process.start()
        await process.wait()
        self.assertEqual(await process.returncode(), 3)

    async def test_async_returncode_is_none_while_the_child_runs(self) -> None:
        process = _async_sleeper()
        await process.start()
        self.assertIsNone(await process.returncode())
        await process.kill()
        await process.wait()

    async def test_async_terminate_ends_the_child_like_kill(self) -> None:
        process = _async_sleeper()
        await process.start()
        await process.terminate()
        self.assertNotEqual(await process.wait(), 0)

    async def test_async_close_is_idempotent_and_releases_the_handle(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", "pass"])
        await process.start()
        await process.close()
        await process.close()
        with self.assertRaises(RuntimeError):
            await process.wait()

    async def test_async_close_before_start_succeeds(self) -> None:
        await AsyncRunningProcess(sys.executable, ["-c", "pass"]).close()


class TestProcessLifecycleSyncParity(unittest.TestCase):
    """The sync half of the rows above."""

    def test_sync_poll_reports_none_while_running_and_a_code_after_exit(self) -> None:
        process = RunningProcess([sys.executable, "-c", SLEEP_SCRIPT], auto_run=False)
        process.start()
        self.assertIsNone(process.poll())
        process.kill()
        process.wait()
        self.assertIsNotNone(process.poll())

    def test_sync_returncode_matches_the_code_the_child_chose(self) -> None:
        process = RunningProcess([sys.executable, "-c", EXIT_SCRIPT], auto_run=False)
        process.start()
        process.wait()
        self.assertEqual(process.returncode, 3)

    def test_sync_returncode_is_none_while_the_child_runs(self) -> None:
        process = RunningProcess([sys.executable, "-c", SLEEP_SCRIPT], auto_run=False)
        process.start()
        self.assertIsNone(process.returncode)
        process.kill()
        process.wait()

    def test_sync_terminate_ends_the_child_like_kill(self) -> None:
        process = RunningProcess([sys.executable, "-c", SLEEP_SCRIPT], auto_run=False)
        process.start()
        process.terminate()
        self.assertNotEqual(process.wait(), 0)

    def test_sync_close_is_idempotent(self) -> None:
        process = RunningProcess([sys.executable, "-c", "pass"], auto_run=False)
        process.start()
        process.wait()
        process.close()
        process.close()


class TestProcessGroupAndTreeParity(unittest.IsolatedAsyncioTestCase):
    async def test_async_terminate_group_soft_is_a_noop_without_an_owned_group(
        self,
    ) -> None:
        process = _async_sleeper()
        await process.start()
        self.assertFalse(
            await process.terminate_group_soft(),
            "without create_process_group there is no group to signal",
        )
        await process.kill()
        await process.wait()

    async def test_async_terminate_group_soft_signals_an_owned_group(self) -> None:
        process = _async_sleeper(create_process_group=True)
        await process.start()
        self.assertTrue(await process.terminate_group_soft())
        # Ctrl+Break is advisory on Windows and depends on the console the test
        # harness provides, so only delivery is contracted; cleanup is
        # best-effort because the signal may already have ended the child.
        try:
            await process.wait_timeout(10.0)
        except RuntimeError:
            pass
        try:
            await process.kill()
        except RuntimeError:
            pass

    async def test_async_terminate_group_soft_before_start_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            await _async_sleeper(create_process_group=True).terminate_group_soft()

    async def test_async_kill_tree_reports_the_instances_it_killed(self) -> None:
        process = _async_sleeper()
        await process.start()
        killed = await process.kill_tree(5.0)
        self.assertGreaterEqual(killed, 1)
        self.assertNotEqual(await process.wait(), 0)

    async def test_module_level_kill_process_tree_matches_the_sync_helper(
        self,
    ) -> None:
        # A pid that cannot exist: both forms treat a missing tree as a
        # successful no-op rather than an error.
        missing = 0xFFFFFFFF
        self.assertIsNone(sync_kill_process_tree(missing, timeout_seconds=0.1))
        self.assertEqual(await kill_process_tree(missing, timeout_seconds=0.1), 0)

    async def test_module_level_kill_process_tree_kills_a_real_child(self) -> None:
        process = _async_sleeper()
        await process.start()
        pid = await process.pid()
        self.assertGreaterEqual(await kill_process_tree(pid, timeout_seconds=5.0), 1)
        self.assertNotEqual(await process.wait(), 0)


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestPtyParity(unittest.IsolatedAsyncioTestCase):
    def _async_pty(self) -> AsyncPseudoTerminalProcess:
        return AsyncPseudoTerminalProcess(_pty_argv())

    async def test_async_pty_send_interrupt_before_start_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            await self._async_pty().send_interrupt()

    async def test_async_pty_respond_to_queries_without_a_query_is_a_noop(self) -> None:
        await self._async_pty().respond_to_queries(b"plain output")

    async def test_async_pty_tree_termination_is_accepted_after_start(self) -> None:
        process = self._async_pty()
        await process.start()
        await process.terminate_tree()
        await process.kill_tree()
        await process.close()

    async def test_async_pty_wait_and_drain_agrees_with_wait(self) -> None:
        process = self._async_pty()
        await process.start()
        await process.kill()
        code = await process.wait_and_drain(20.0, 1.0)
        self.assertEqual(code, await process.wait(5.0))
        await process.close()

    async def test_async_pty_wait_for_reader_closed_is_bounded(self) -> None:
        process = self._async_pty()
        await process.start()
        # Whether the reader has closed yet is timing-dependent; what is
        # contracted is that the call returns rather than hanging.
        self.assertIn(await process.wait_for_reader_closed(0.2), (True, False))
        await process.close()

    async def test_async_pty_echo_state_round_trips(self) -> None:
        process = self._async_pty()
        initial = process.echo_enabled()
        process.set_echo(not initial)
        self.assertEqual(process.echo_enabled(), not initial)
        process.set_echo(initial)
        self.assertEqual(process.echo_enabled(), initial)

    async def test_async_pty_relay_is_inactive_until_started(self) -> None:
        process = self._async_pty()
        self.assertFalse(process.terminal_input_relay_active())
        # Unconditional in teardown paths, so a stop for a relay that never ran
        # must be a no-op rather than an error.
        process.request_terminal_input_relay_stop()
        await process.stop_terminal_input_relay()
        self.assertFalse(process.terminal_input_relay_active())

    async def test_async_pty_start_relay_requires_a_running_pty(self) -> None:
        with self.assertRaises(RuntimeError):
            await self._async_pty().start_terminal_input_relay()

    async def test_async_pty_metrics_track_recorded_input(self) -> None:
        process = self._async_pty()
        self.assertEqual(process.pty_input_bytes_total(), 0)
        process.record_input_metrics(b"abc\n", True)
        self.assertEqual(process.pty_input_bytes_total(), 4)
        self.assertEqual(process.pty_newline_events_total(), 1)
        self.assertEqual(process.pty_submit_events_total(), 1)
        self.assertEqual(process.pty_output_bytes_total(), 0)
        self.assertEqual(process.pty_control_churn_bytes_total(), 0)

    async def test_async_pty_store_returncode_and_mark_reader_closed(self) -> None:
        process = self._async_pty()
        process.store_returncode(7)
        process.mark_reader_closed()
        self.assertTrue(await process.wait_for_reader_closed(0.2))

    async def test_async_pty_close_nonblocking_is_safe_before_start(self) -> None:
        process = self._async_pty()
        process.close_nonblocking()
        process.close_nonblocking()


@unittest.skipUnless(Pty.is_available(), "PTY backend unavailable on this host")
class TestPtySyncParity(unittest.TestCase):
    """The sync half of the PTY rows above."""

    def test_sync_pty_send_interrupt_before_start_is_rejected(self) -> None:
        process = PseudoTerminalProcess(_pty_argv(), auto_run=False)
        # RuntimeError specifically -- the async surface raises the same type,
        # which is the parity claim. A blind `Exception` would pass even if the
        # call started failing for an unrelated reason.
        with self.assertRaises(RuntimeError):
            process.send_interrupt()

    def test_sync_pty_terminal_input_relay_is_inactive_until_started(self) -> None:
        process = PseudoTerminalProcess(_pty_argv(), auto_run=False)
        self.assertFalse(process.terminal_input_relay_active)
        process.stop_terminal_input_relay()
        self.assertFalse(process.terminal_input_relay_active)

    def test_sync_pty_output_bytes_start_at_zero(self) -> None:
        process = PseudoTerminalProcess(_pty_argv(), auto_run=False)
        self.assertEqual(process.output_bytes, 0)


class TestInteractiveDispatchParity(unittest.IsolatedAsyncioTestCase):
    async def test_pipe_session_rejects_pty_only_operations(self) -> None:
        session = AsyncInteractiveProcess([sys.executable, "-c", "pass"])
        with self.assertRaises(RuntimeError):
            await session.send_interrupt()

    async def test_pipe_session_kill_tree_reports_a_count(self) -> None:
        session = AsyncInteractiveProcess([sys.executable, "-c", SLEEP_SCRIPT])
        await session.start()
        killed = await session.kill_tree(5.0)
        self.assertIsNotNone(killed)
        self.assertGreaterEqual(killed or 0, 1)
        await session.wait()


if __name__ == "__main__":
    unittest.main()
