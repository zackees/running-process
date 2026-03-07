"""Tests for capture mode, text/binary modes, and bufsize overrides."""

import subprocess
import time
import unittest
from unittest import mock

from running_process import RunningProcess, RunningProcessManagerSingleton


class TestCaptureMode(unittest.TestCase):
    """Tests for capture parameter."""

    def test_capture_true_captures_output(self):
        """With capture=True, output should be captured."""
        rp = RunningProcess(
            ["python", "-c", "print('test output')"],
            capture=True,
            auto_run=True,
        )
        rp.wait()
        self.assertIn("test output", rp.stdout)
        self.assertGreater(len(rp.accumulated_output), 0)

    def test_capture_false_does_not_capture_output(self):
        """With capture=False, output should NOT be captured."""
        rp = RunningProcess(
            ["python", "-c", "print('test output')"],
            capture=False,
            auto_run=True,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")
        self.assertEqual(len(rp.accumulated_output), 0)

    def test_capture_false_with_timeout(self):
        """With capture=False, timeout should still work."""
        start = time.time()
        rp = RunningProcess(
            ["python", "-c", "import time; time.sleep(5)"],
            capture=False,
            timeout=1,
            auto_run=True,
        )
        with self.assertRaises(TimeoutError):
            rp.wait()
        elapsed = time.time() - start
        # Should timeout around 1 second, not complete the full 5 seconds
        self.assertLess(elapsed, 3.0)

    def test_capture_false_non_zero_exit(self):
        """With capture=False, non-zero exit code should be detected."""
        rp = RunningProcess(
            ["python", "-c", "import sys; sys.exit(42)"],
            capture=False,
            check=False,
            auto_run=True,
        )
        exit_code = rp.wait()
        self.assertEqual(exit_code, 42)

    def test_capture_false_check_mode(self):
        """With capture=False and check=True, exit code should be detectable."""
        # Note: check behavior is implemented in run() method, not wait()
        rp = RunningProcess(
            ["python", "-c", "import sys; sys.exit(1)"],
            capture=False,
            check=True,
            auto_run=True,
        )
        exit_code = rp.wait()
        # Check is stored but not enforced by wait() - that's handled by run()
        self.assertEqual(exit_code, 1)

    def test_capture_true_default(self):
        """Capture should default to True."""
        rp = RunningProcess(
            ["python", "-c", "print('default capture')"],
            auto_run=True,
        )
        rp.wait()
        self.assertIn("default capture", rp.stdout)

    def test_capture_false_with_manual_start(self):
        """With capture=False and auto_run=False, manual start should work."""
        rp = RunningProcess(
            ["python", "-c", "print('manual start')"],
            capture=False,
            auto_run=False,
        )
        rp.start()
        rp.wait()
        self.assertEqual(rp.stdout, "")


class TestTextBinaryMode(unittest.TestCase):
    """Tests for text vs binary mode."""

    def test_text_mode_default(self):
        """Text mode should be the default."""
        rp = RunningProcess(
            ["python", "-c", "print('text mode')"],
            auto_run=True,
        )
        rp.wait()
        # Output should be strings
        output = rp.stdout
        self.assertIsInstance(output, str)
        self.assertIn("text mode", output)

    def test_binary_mode_returns_empty_when_not_capturing(self):
        """Binary mode with capture=False should return empty output."""
        rp = RunningProcess(
            ["python", "-c", "import sys; sys.stdout.buffer.write(b'binary')"],
            text=False,
            capture=False,
            auto_run=True,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")

    def test_text_mode_explicit(self):
        """Explicitly setting text=True should work."""
        rp = RunningProcess(
            ["python", "-c", "print('explicit text')"],
            text=True,
            auto_run=True,
        )
        rp.wait()
        self.assertIn("explicit text", rp.stdout)
        self.assertIsInstance(rp.stdout, str)

    def test_encoding_ignored_in_binary_mode(self):
        """When text=False, encoding parameter should be ignored."""
        # This should not raise an error even though encoding is ignored
        rp = RunningProcess(
            ["python", "-c", "print('test')"],
            text=False,
            encoding="utf-8",  # Should be ignored
            capture=True,
            auto_run=True,
        )
        rp.wait()
        # Should not crash

    def test_errors_ignored_in_binary_mode(self):
        """When text=False, errors parameter should be ignored."""
        # This should not raise an error even though errors is ignored
        rp = RunningProcess(
            ["python", "-c", "print('test')"],
            text=False,
            errors="replace",  # Should be ignored
            capture=True,
            auto_run=True,
        )
        rp.wait()
        # Should not crash

    def test_text_false_capture_true(self):
        """Binary mode with capture=True should work (though bytes might not iterate well)."""
        rp = RunningProcess(
            ["python", "-c", "print('bytes test')"],
            text=False,
            capture=True,
            auto_run=True,
        )
        # Should complete without error
        rp.wait()


class TestBufsizeOverride(unittest.TestCase):
    """Tests for bufsize override behavior."""

    def test_bufsize_1_silent_override(self):
        """Setting bufsize=1 should not produce warnings."""
        with mock.patch("running_process.running_process.logger") as mock_logger:
            rp = RunningProcess(
                ["python", "-c", "print('test')"],
                bufsize=1,
                auto_run=True,
            )
            rp.wait()
            # Should not warn about bufsize override
            for call in mock_logger.warning.call_args_list:
                if call:
                    msg = str(call)
                    self.assertNotIn("bufsize", msg.lower())

    def test_bufsize_non_1_disables_pythonunbuffered(self):
        """Setting bufsize != 1 should warn and disable PYTHONUNBUFFERED."""
        with mock.patch("running_process.running_process.logger") as mock_logger:
            rp = RunningProcess(
                ["python", "-c", "print('test')"],
                bufsize=4096,  # Non-1 value
                auto_run=True,
            )
            rp.wait()
            # Should warn about bufsize override
            warning_found = False
            for call in mock_logger.warning.call_args_list:
                if call and "bufsize" in str(call):
                    warning_found = True
                    self.assertIn("4096", str(call))
            self.assertTrue(warning_found, "Should warn about bufsize override")

    def test_bufsize_0_disables_pythonunbuffered(self):
        """Setting bufsize=0 should warn and disable PYTHONUNBUFFERED."""
        with mock.patch("running_process.running_process.logger") as mock_logger:
            rp = RunningProcess(
                ["python", "-c", "print('test')"],
                bufsize=0,
                auto_run=True,
            )
            rp.wait()
            # Should warn
            warning_found = False
            for call in mock_logger.warning.call_args_list:
                if call and "bufsize" in str(call):
                    warning_found = True
            self.assertTrue(warning_found)

    def test_bufsize_negative_1_disables_pythonunbuffered(self):
        """Setting bufsize=-1 should warn and disable PYTHONUNBUFFERED."""
        with mock.patch("running_process.running_process.logger") as mock_logger:
            rp = RunningProcess(
                ["python", "-c", "print('test')"],
                bufsize=-1,
                auto_run=True,
            )
            rp.wait()
            # Should warn
            warning_found = False
            for call in mock_logger.warning.call_args_list:
                if call and "bufsize" in str(call):
                    warning_found = True
            self.assertTrue(warning_found)

    def test_default_bufsize_preserves_optimization(self):
        """Without bufsize override, PYTHONUNBUFFERED should be set."""
        # This is implicitly tested by other tests, but we verify output works
        rp = RunningProcess(
            ["python", "-c", "print('optimized')"],
            auto_run=True,
        )
        rp.wait()
        self.assertIn("optimized", rp.stdout)


class TestCombinedModes(unittest.TestCase):
    """Tests for combinations of capture, text, and bufsize."""

    def test_capture_false_with_text_false(self):
        """capture=False + text=False should work."""
        rp = RunningProcess(
            ["python", "-c", "print('combo test')"],
            capture=False,
            text=False,
            auto_run=True,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")

    def test_capture_true_with_bufsize_override(self):
        """capture=True + bufsize override should work."""
        rp = RunningProcess(
            ["python", "-c", "print('combo bufsize')"],
            capture=True,
            bufsize=512,
            auto_run=True,
        )
        rp.wait()
        self.assertIn("combo bufsize", rp.stdout)

    def test_text_false_capture_true_bufsize_override(self):
        """All three overrides should work together."""
        rp = RunningProcess(
            ["python", "-c", "print('all overrides')"],
            text=False,
            capture=True,
            bufsize=1024,
            auto_run=True,
        )
        rp.wait()
        # Should complete without error

    def test_capture_false_bufsize_override(self):
        """capture=False + bufsize override should work."""
        rp = RunningProcess(
            ["python", "-c", "print('no capture with bufsize')"],
            capture=False,
            bufsize=2048,
            auto_run=True,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")

    def test_multiline_output_capture_true(self):
        """Multi-line output should be fully captured with capture=True."""
        rp = RunningProcess(
            [
                "python",
                "-c",
                "print('line1'); print('line2'); print('line3')",
            ],
            capture=True,
            auto_run=True,
        )
        rp.wait()
        self.assertIn("line1", rp.stdout)
        self.assertIn("line2", rp.stdout)
        self.assertIn("line3", rp.stdout)

    def test_multiline_output_capture_false(self):
        """Multi-line output should not be captured with capture=False."""
        rp = RunningProcess(
            [
                "python",
                "-c",
                "print('line1'); print('line2'); print('line3')",
            ],
            capture=False,
            auto_run=True,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")
        self.assertEqual(rp.accumulated_output, [])


class TestProtectedKeysWithCapture(unittest.TestCase):
    """Tests that stdout/stderr remain protected even with capture modes."""

    def test_cannot_override_stdout(self):
        """stdout should still be protected."""
        # Trying to override stdout should be silently ignored by the merge logic
        rp = RunningProcess(
            ["python", "-c", "print('test')"],
            capture=True,
            stdout=subprocess.DEVNULL,  # Try to override
            auto_run=True,
        )
        rp.wait()
        # Should still capture output (stdout override ignored)
        self.assertIn("test", rp.stdout)

    def test_cannot_override_stderr(self):
        """stderr should still be protected."""
        # Trying to override stderr should be silently ignored
        rp = RunningProcess(
            ["python", "-c", "import sys; print('out'); print('err', file=sys.stderr)"],
            capture=True,
            stderr=subprocess.DEVNULL,  # Try to override
            auto_run=True,
        )
        rp.wait()
        # Both stdout and stderr should be captured (merged)
        self.assertIn("out", rp.stdout)


class TestRunStaticMethod(unittest.TestCase):
    """Tests for the RunningProcess.run() static method."""

    def test_run_capture_output_true_captures_stdout(self):
        """capture_output=True should capture and return stdout."""
        result = RunningProcess.run(
            ["python", "-c", "print('captured output')"],
            capture_output=True,
        )
        self.assertIsNotNone(result.stdout)
        self.assertIn("captured output", result.stdout)

    def test_run_capture_output_false_returns_none_stdout(self):
        """capture_output=False should return None for stdout."""
        result = RunningProcess.run(
            ["python", "-c", "print('this should not be captured')"],
            capture_output=False,
        )
        self.assertIsNone(result.stdout)

    def test_run_stdout_pipe_captures_stdout(self):
        """stdout=subprocess.PIPE should be treated as capture=True."""
        result = RunningProcess.run(
            ["python", "-c", "print('piped output')"],
            stdout=subprocess.PIPE,
        )
        self.assertIsNotNone(result.stdout)
        self.assertIn("piped output", result.stdout)

    def test_run_default_behavior_is_no_capture(self):
        """Default call without capture_output should return None for stdout."""
        result = RunningProcess.run(
            ["python", "-c", "print('default no capture')"],
        )
        self.assertIsNone(result.stdout)

    def test_run_capture_registers_with_manager(self):
        """capture_output=True path should register with manager."""
        with mock.patch.object(RunningProcessManagerSingleton, "register") as mock_register:
            RunningProcess.run(
                ["python", "-c", "print('manager test')"],
                capture_output=True,
            )
            # Should have registered the process
            mock_register.assert_called()

    def test_run_no_capture_registers_with_manager(self):
        """capture_output=False path should register with manager."""
        with mock.patch.object(RunningProcessManagerSingleton, "register") as mock_register:
            RunningProcess.run(
                ["python", "-c", "print('manager test no capture')"],
                capture_output=False,
            )
            # Should have registered the process
            mock_register.assert_called()

    def test_run_check_raises_on_nonzero_with_capture(self):
        """check=True with capture_output=True should raise CalledProcessError."""
        with self.assertRaises(subprocess.CalledProcessError):
            RunningProcess.run(
                ["python", "-c", "import sys; sys.exit(42)"],
                capture_output=True,
                check=True,
            )

    def test_run_check_raises_on_nonzero_no_capture(self):
        """check=True with capture_output=False should raise CalledProcessError."""
        with self.assertRaises(subprocess.CalledProcessError):
            RunningProcess.run(
                ["python", "-c", "import sys; sys.exit(42)"],
                capture_output=False,
                check=True,
            )

    def test_run_timeout_raises_with_capture(self):
        """timeout exceeded with capture_output=True should raise TimeoutExpired."""
        with self.assertRaises(subprocess.TimeoutExpired):
            RunningProcess.run(
                ["python", "-c", "import time; time.sleep(5)"],
                capture_output=True,
                timeout=0.5,
            )

    def test_run_timeout_raises_no_capture(self):
        """timeout exceeded with capture_output=False should raise TimeoutExpired."""
        with self.assertRaises(subprocess.TimeoutExpired):
            RunningProcess.run(
                ["python", "-c", "import time; time.sleep(5)"],
                capture_output=False,
                timeout=0.5,
            )

    def test_run_manager_unregisters_after_completion_capture(self):
        """After completion with capture_output=True, process should not be in active list."""
        manager = RunningProcessManagerSingleton
        initial_count = len(manager.list_active())

        RunningProcess.run(
            ["python", "-c", "print('test')"],
            capture_output=True,
        )

        final_count = len(manager.list_active())
        self.assertEqual(initial_count, final_count)

    def test_run_manager_unregisters_after_completion_no_capture(self):
        """After completion with capture_output=False, process should not be in active list."""
        manager = RunningProcessManagerSingleton
        initial_count = len(manager.list_active())

        RunningProcess.run(
            ["python", "-c", "print('test')"],
            capture_output=False,
        )

        final_count = len(manager.list_active())
        self.assertEqual(initial_count, final_count)


if __name__ == "__main__":
    unittest.main()
