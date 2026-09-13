"""Regression tests for non-vacuous native output-shutdown CI evidence."""

from __future__ import annotations

import unittest

from ci.output_shutdown_native import required_tests, validate_passes


class OutputShutdownNativeTests(unittest.TestCase):
    def test_windows_requires_the_queued_before_syscall_race(self) -> None:
        cases = required_tests("win32")
        self.assertEqual(len(cases["running-process"]), 6)
        self.assertEqual(len(cases["running-process-platform-internal"]), 3)
        self.assertTrue(
            any(
                "queued_before_its_syscall" in name
                for name in cases["running-process-platform-internal"]
            )
        )

    def test_other_hosts_require_platform_read_cleanup(self) -> None:
        for host in ("linux", "darwin"):
            self.assertEqual(
                len(required_tests(host)["running-process-platform-internal"]), 2
            )

    def test_missing_or_skipped_tests_cannot_pass(self) -> None:
        for output in (
            "",
            "Summary 0 tests run: 0 passed",
            "SKIP [0.1s] sample cleanup",
            "PASS [0.1s] other cleanup",
        ):
            with self.subTest(output=output), self.assertRaises(ValueError):
                validate_passes(output, "sample", ("cleanup",))

    def test_every_exact_name_must_pass(self) -> None:
        output = "PASS [0.1s] (1/2) sample cleanup\nPASS [0.1s] sample retry\n"
        validate_passes(output, "sample", ("cleanup", "retry"))
        with self.assertRaises(ValueError):
            validate_passes(output, "sample", ("clean",))

    def test_soldr_timestamped_passes_are_recognized(self) -> None:
        validate_passes(
            "    4.23         PASS [0.006s] (1/2) sample cleanup\n",
            "sample",
            ("cleanup",),
        )


if __name__ == "__main__":
    unittest.main()
