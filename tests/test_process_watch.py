from __future__ import annotations

import sys
import unittest
from math import inf, nan
from pathlib import Path

from running_process import (
    ObservationGrade,
    ObservationPolicy,
    ProcessObservationUnavailableError,
    ProcessWatch,
    ProcessWatchConfigurationError,
    RunningProcess,
    StackCapture,
    StackDump,
)
from running_process.asyncio import AsyncRunningProcess


class ProcessWatchConfigurationTests(unittest.TestCase):
    def test_exec_basename_is_explicit_and_immutable(self) -> None:
        watch = ProcessWatch.on_exec(
            basename="soldr",
            dump=StackDump(capture=StackCapture.ORIGIN_REQUIRED),
            label="recursive-soldr",
        )
        self.assertEqual(watch.basename, "soldr")
        self.assertEqual(watch.limit, 1)
        with self.assertRaises(AttributeError):
            watch.basename = "other"  # type: ignore[misc]

    def test_exec_rejects_path_and_basename(self) -> None:
        with self.assertRaises(ProcessWatchConfigurationError):
            ProcessWatch.on_exec("soldr", path=Path("/usr/bin/soldr"))

    def test_exit_rejects_code_and_signal(self) -> None:
        with self.assertRaises(ProcessWatchConfigurationError):
            ProcessWatch.on_exit(1, signal=9)

    def test_unbounded_watch_is_explicit(self) -> None:
        watch = ProcessWatch.on_failure(limit=None)
        self.assertIsNone(watch.limit)

    def test_cooldown_must_be_finite(self) -> None:
        for value in (nan, inf, -inf):
            with self.subTest(value=value), self.assertRaises(
                ProcessWatchConfigurationError
            ):
                ProcessWatch.on_spawn(cooldown_seconds=value)


class ProcessWatchNativeTests(unittest.TestCase):
    def test_capability_report_is_honest_for_host(self) -> None:
        capability = RunningProcess.process_observation_capabilities()
        self.assertTrue(capability.exact_backend)
        self.assertTrue(capability.reason)
        self.assertEqual(capability.exact_available, sys.platform.startswith("linux"))

    @unittest.skipIf(sys.platform.startswith("linux"), "unsupported-host contract")
    def test_require_exact_fails_before_start_on_unsupported_host(self) -> None:
        with self.assertRaises(ProcessObservationUnavailableError):
            RunningProcess(
                [sys.executable, "-c", "pass"],
                auto_run=False,
                process_watches=[ProcessWatch.on_spawn()],
                process_observation=ObservationPolicy.REQUIRE_EXACT,
            )

    def test_non_invasive_selection_never_claims_exact(self) -> None:
        process = RunningProcess(
            [sys.executable, "-c", "pass"],
            auto_run=False,
            process_watches=[ProcessWatch.on_spawn()],
            process_observation=ObservationPolicy.NON_INVASIVE,
        )
        observation = process.process_observation
        self.assertIsNotNone(observation)
        assert observation is not None
        self.assertNotIn(
            observation.observation_grade,
            (ObservationGrade.EXACT_TRACE, ObservationGrade.EXACT_EVENT),
        )

    def test_add_watch_is_pre_start_only(self) -> None:
        process = RunningProcess([sys.executable, "-c", "pass"], auto_run=False)
        process.add_process_watch(ProcessWatch.on_exec(basename=Path(sys.executable).name))
        process.start()
        process.wait()
        with self.assertRaises(RuntimeError):
            process.add_process_watch(ProcessWatch.on_spawn(label="late"))


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux exact trace")
class AsyncProcessWatchTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_cursor_is_opened_before_exact_launch(self) -> None:
        process = AsyncRunningProcess(
            sys.executable,
            ("-c", "pass"),
            process_watches=[ProcessWatch.on_exec(label="root-exec")],
            process_observation=ObservationPolicy.REQUIRE_EXACT,
        )
        cursor = process.process_watch_cursor()
        second_cursor = process.process_watch_cursor()
        await process.start()
        self.assertEqual(await process.wait(), 0)
        match = await cursor.read_next()
        self.assertIsNotNone(match)
        assert match is not None
        self.assertEqual(match.watch.label, "root-exec")
        second_match = await second_cursor.read_next()
        self.assertIsNotNone(second_match)
        assert second_match is not None
        self.assertEqual(second_match.sequence, match.sequence)
        self.assertIsNone(await cursor.read_next())
        self.assertIsNone(await second_cursor.read_next())


if __name__ == "__main__":
    unittest.main()
