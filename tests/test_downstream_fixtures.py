"""Downstream consumer fixtures for #875.

FastLED (`~/dev/fastled`) uses this library through the legacy synchronous
imports only. #875's compatibility promise is that finishing async support does
not change what that consumer sees, so these fixtures exercise the shapes a
sync-only consumer actually uses and assert the properties that would break it:

* the legacy import path still resolves every name it depends on
* a sync-only workflow runs without an asyncio event loop ever existing
* sync and async use in the *same* process do not deadlock, panic on a nested
  runtime, or start a second library runtime

The event-loop assertions are the load-bearing ones. If the sync path ever
started an event loop to service itself, a consumer that already runs its own
loop would get a nested-runtime failure at import-or-call time -- exactly the
class of breakage the "no async-to-sync bridge" rule exists to prevent.
"""

from __future__ import annotations

import asyncio
import subprocess
import sys
import textwrap
import unittest

FIXTURE_SYNC_ONLY = textwrap.dedent(
    """
    # A FastLED-shaped consumer: legacy imports, synchronous calls, no asyncio.
    import asyncio
    import sys


    class _NoLoopPolicy(asyncio.DefaultEventLoopPolicy):
        # Booby-trap the only route by which a loop gets built. Inspecting the
        # policy's stored loop *afterwards* does not work: `new_event_loop()`
        # creates a loop without installing it, so the stored slot stays None
        # and the check passes no matter what happened.
        def new_event_loop(self):
            raise AssertionError("the synchronous path created an event loop")


    asyncio.set_event_loop_policy(_NoLoopPolicy())

    from running_process import (
        RunningProcess,
        get_process_tree_info,
        kill_process_tree,
        subprocess_run,
    )

    process = RunningProcess([sys.executable, "-c", "print('downstream')"])
    assert process.wait() == 0, "sync wait must report the child's exit code"
    assert "downstream" in str(process.stdout), process.stdout

    completed = subprocess_run(
        [sys.executable, "-c", "print('module-helper')"],
        cwd=None,
        check=True,
        timeout=60,
    )
    assert completed.returncode == 0
    assert "module-helper" in completed.stdout

    # Module-level helpers a consumer calls for cleanup.
    import os

    assert isinstance(get_process_tree_info(os.getpid()), str)
    kill_process_tree(0xFFFFFFFF, timeout_seconds=0.1)

    # Reaching here means the booby-trap above never fired: the whole sync
    # workflow ran without an event loop being constructed.
    print("SYNC_ONLY_OK")
    """
)

FIXTURE_MIXED = textwrap.dedent(
    """
    # Sync and async use in one process, in both orders.
    import asyncio
    import sys

    from running_process import RunningProcess
    from running_process.asyncio import AsyncRunningProcess


    def sync_leg(tag):
        process = RunningProcess([sys.executable, "-c", f"print('{tag}')"])
        assert process.wait() == 0
        return str(process.stdout)


    async def async_leg(tag):
        process = AsyncRunningProcess(sys.executable, ["-c", f"print('{tag}')"])
        code, stdout, _stderr = await process.run()
        assert code == 0
        return stdout.decode()


    # sync first, then async
    assert "before" in sync_leg("before")
    assert "async-1" in asyncio.run(async_leg("async-1"))
    # async first, then sync -- the ordering that would expose a runtime the
    # async leg left behind in a state the sync leg cannot use.
    assert "async-2" in asyncio.run(async_leg("async-2"))
    assert "after" in sync_leg("after")


    # And sync work from inside a running loop: the sync API must not need the
    # loop's thread, and must not try to enter a second runtime.
    async def sync_inside_a_running_loop():
        return sync_leg("inside-loop")


    assert "inside-loop" in asyncio.run(sync_inside_a_running_loop())
    print("MIXED_OK")
    """
)


def _run_fixture(source: str) -> subprocess.CompletedProcess[str]:
    """Run a fixture in a *fresh* interpreter.

    In-process would prove nothing: the test session has already imported
    asyncio and may already have a loop, so "no loop was created" is only
    meaningful in a process that started clean.
    """
    return subprocess.run(
        [sys.executable, "-c", source],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


class TestSyncOnlyDownstream(unittest.TestCase):
    def test_legacy_imports_all_resolve(self) -> None:
        # Named individually rather than via a wildcard so a removal names the
        # symbol that went missing.
        import running_process

        for name in (
            "RunningProcess",
            "PseudoTerminalProcess",
            "InteractiveProcess",
            "subprocess_run",
            "kill_process_tree",
            "terminate_process_tree",
            "get_process_tree_info",
            "launch_detached",
            "ExitStatus",
            "CpuPriority",
        ):
            self.assertTrue(
                hasattr(running_process, name),
                f"legacy export {name!r} disappeared from running_process",
            )

    def test_sync_only_consumer_runs_without_an_event_loop(self) -> None:
        result = _run_fixture(FIXTURE_SYNC_ONLY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("SYNC_ONLY_OK", result.stdout)

    def test_the_no_event_loop_check_actually_fires(self) -> None:
        """Control for the test above.

        The first version of that assertion inspected the policy's stored loop
        after the fact, which never fired: `new_event_loop()` builds a loop
        without installing it, so the slot stayed empty regardless. This runs
        the same fixture with a deliberate loop creation spliced in and
        requires it to fail -- without this, a green result up there could mean
        "no loop was created" or "the check is broken", and those look
        identical.
        """
        # The fixture is dedented, so the anchor sits at column zero.
        sabotaged = FIXTURE_SYNC_ONLY.replace(
            'print("SYNC_ONLY_OK")',
            'asyncio.new_event_loop()\nprint("SYNC_ONLY_OK")',
        )
        self.assertNotEqual(
            sabotaged, FIXTURE_SYNC_ONLY, "the control failed to splice anything in"
        )
        result = _run_fixture(sabotaged)
        self.assertNotEqual(
            result.returncode, 0, "creating an event loop must fail the fixture"
        )
        self.assertIn("created an event loop", result.stderr)

    def test_async_module_is_not_imported_by_the_legacy_surface(self) -> None:
        # Importing `running_process` must not drag in the async facade. If it
        # did, a consumer on a build without the async feature would fail at
        # import rather than at the call they never make.
        result = _run_fixture(
            "import running_process, sys;"
            " assert 'running_process.asyncio' not in sys.modules,"
            " 'legacy import pulled in the async facade';"
            " print('NO_ASYNC_IMPORT_OK')"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NO_ASYNC_IMPORT_OK", result.stdout)


class TestMixedUseDownstream(unittest.TestCase):
    def test_sync_and_async_coexist_in_one_process(self) -> None:
        result = _run_fixture(FIXTURE_MIXED)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("MIXED_OK", result.stdout)


class TestSyncWaitDoesNotHoldTheLoop(unittest.IsolatedAsyncioTestCase):
    async def test_sync_wait_inside_a_running_loop_lets_the_loop_progress(self) -> None:
        """A sync wait must release the GIL, not freeze the event loop.

        This is the observable form of the GIL-release requirement: if
        `RunningProcess.wait` held the GIL for its duration, the heartbeat task
        below could not tick while the wait was in flight.
        """
        from running_process import RunningProcess

        ticks = 0

        async def heartbeat() -> None:
            nonlocal ticks
            while True:
                await asyncio.sleep(0.01)
                ticks += 1

        beat = asyncio.create_task(heartbeat())
        try:
            await asyncio.sleep(0.05)
            before = ticks
            process = RunningProcess(
                [sys.executable, "-c", "import time; time.sleep(0.5)"]
            )
            await asyncio.get_running_loop().run_in_executor(None, process.wait)
            self.assertGreater(
                ticks,
                before,
                "the event loop made no progress during a sync wait, so the "
                "wait held the GIL",
            )
        finally:
            beat.cancel()

    async def test_cancelling_an_async_wait_resumes_the_owning_loop(self) -> None:
        """Cancellation must return control to the loop that owns the task."""
        from running_process.asyncio import AsyncRunningProcess

        process = AsyncRunningProcess(
            sys.executable, ["-c", "import time; time.sleep(60)"]
        )
        await process.start()
        waiter = asyncio.create_task(process.wait())
        await asyncio.sleep(0.1)
        waiter.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await waiter
        # The loop is still usable, and the process is still addressable: a
        # cancelled wait must not have taken the actor down with it.
        await asyncio.sleep(0)
        await process.kill()
        await process.wait()


if __name__ == "__main__":
    unittest.main()
