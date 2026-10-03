"""PID cleanup evidence and CLI diagnostics share an explicit writable root."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from running_process.dump_paths import (
    RUNNING_PROCESS_STACK_DUMP_DIR_ENV,
    stack_dump_dir,
)
from tests import pid_tracker


class TestPidTrackerLogDirectory(unittest.TestCase):
    def test_explicit_diagnostic_root_holds_pid_cleanup_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            with patch.dict(os.environ, {RUNNING_PROCESS_STACK_DUMP_DIR_ENV: scratch}):
                pid_tracker.reset_log()
                # A synthetic PID stays only in this isolated temporary log.
                pid_tracker.record_pid(2147483647)
                self.assertEqual(pid_tracker._read_pids(), [2147483647])
                self.assertEqual(stack_dump_dir(), root)
                self.assertEqual(pid_tracker._pid_log().parent, root)
                self.assertEqual(
                    (root / "test-spawned-pids.log").read_text(), "2147483647\n"
                )

    def test_unconfigured_host_keeps_the_repository_log_directory(self) -> None:
        with patch.dict(os.environ, {RUNNING_PROCESS_STACK_DUMP_DIR_ENV: ""}):
            self.assertEqual(
                pid_tracker._pid_log(),
                Path(pid_tracker.__file__).resolve().parent.parent
                / "logs"
                / "test-spawned-pids.log",
            )
