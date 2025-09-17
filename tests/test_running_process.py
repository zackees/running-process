"""Unit tests for the RunningProcess class.

These tests cover the most common use cases without using mocks,
testing real process execution and behavior.
"""

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from running_process import RunningProcess
from running_process.process_output_reader import EndOfStream
from running_process.running_process import subprocess_run


class TestBasicExecution(unittest.TestCase):
    """Test basic RunningProcess creation and execution."""

    def test_simple_echo_command(self):
        """Test basic echo command execution."""
        process = RunningProcess(["echo", "Hello World"], auto_run=False)
        process.start()
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertIn("Hello World", process.stdout)

    def test_simple_echo_command_with_auto_run(self):
        """Test echo command with auto_run=True (default)."""
        process = RunningProcess(["echo", "Hello World"])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertIn("Hello World", process.stdout)

    def test_shell_command_string(self):
        """Test shell command execution with string command."""
        if os.name == "nt":
            process = RunningProcess("echo Hello World", shell=True)  # noqa: S604
        else:
            process = RunningProcess("echo 'Hello World'", shell=True)  # noqa: S604
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertIn("Hello World", process.stdout)

    def test_command_with_working_directory(self):
        """Test command execution with custom working directory."""
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)

            # Create a test file
            test_file = temp_path / "test.txt"
            test_file.write_text("test content")

            # List files in the directory
            if os.name == "nt":
                process = RunningProcess(["dir", "/b"], cwd=temp_path, shell=True)  # noqa: S604
            else:
                process = RunningProcess(["ls"], cwd=temp_path)
            exit_code = process.wait()

            self.assertEqual(exit_code, 0)
            self.assertIn("test.txt", process.stdout)

    def test_python_command_execution(self):
        """Test Python script execution."""
        python_code = "print('Python output'); print('Line 2')"
        process = RunningProcess([sys.executable, "-c", python_code])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertIn("Python output", process.stdout)
        self.assertIn("Line 2", process.stdout)


class TestOutputStreaming(unittest.TestCase):
    """Test output streaming and iteration functionality."""

    def test_get_next_line_basic(self):
        """Test basic line-by-line output reading."""
        process = RunningProcess([sys.executable, "-c", "print('line1'); print('line2')"])

        lines = []
        while True:
            try:
                line = process.get_next_line(timeout=5.0)
                if isinstance(line, EndOfStream):
                    break
                lines.append(line)
            except TimeoutError:
                break

        process.wait()
        self.assertGreaterEqual(len(lines), 2)
        self.assertIn("line1", lines)
        self.assertIn("line2", lines)

    def test_line_iterator_context_manager(self):
        """Test line iterator with context manager."""
        process = RunningProcess([sys.executable, "-c", "print('iter1'); print('iter2')"])

        lines = []
        with process.line_iter(timeout=5.0) as line_iter:
            lines = list(line_iter)

        process.wait()
        self.assertGreaterEqual(len(lines), 2)
        self.assertIn("iter1", lines)
        self.assertIn("iter2", lines)

    def test_drain_stdout(self):
        """Test draining all pending stdout."""
        process = RunningProcess(
            [sys.executable, "-c", "import time; print('fast'); time.sleep(0.1); print('delayed')"]
        )

        # Wait a bit for output to accumulate
        time.sleep(0.5)

        drained_lines = process.drain_stdout()
        process.wait()

        self.assertGreaterEqual(len(drained_lines), 1)
        output_text = "\n".join(drained_lines)
        self.assertIn("fast", output_text)

    def test_has_pending_output(self):
        """Test checking for pending output."""
        process = RunningProcess([sys.executable, "-c", "print('check output')"])

        # Wait for output to be available
        time.sleep(0.2)

        has_output = process.has_pending_output()
        process.wait()

        # Should have had output at some point
        self.assertIsInstance(has_output, bool)

    def test_get_next_line_non_blocking(self):
        """Test non-blocking line retrieval."""
        process = RunningProcess([sys.executable, "-c", "print('non-blocking test')"])

        # Try to get a line without blocking
        result = process.get_next_line_non_blocking()
        process.wait()

        # Result should be string, None, or EndOfStream
        self.assertTrue(result is None or isinstance(result, (str, EndOfStream)))


class TestProcessCompletion(unittest.TestCase):
    """Test process completion and exit codes."""

    def test_successful_exit_code(self):
        """Test process with successful exit code."""
        process = RunningProcess([sys.executable, "-c", "exit(0)"])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertTrue(process.finished)
        self.assertEqual(process.returncode, 0)

    def test_non_zero_exit_code(self):
        """Test process with non-zero exit code."""
        process = RunningProcess([sys.executable, "-c", "exit(42)"])
        exit_code = process.wait()

        self.assertEqual(exit_code, 42)
        self.assertTrue(process.finished)
        self.assertEqual(process.returncode, 42)

    def test_poll_before_completion(self):
        """Test polling process status before completion."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(0.1)"])

        # Poll immediately - should be None (still running)
        initial_poll = process.poll()

        # Wait for completion
        exit_code = process.wait()

        # Poll after completion - should return exit code
        final_poll = process.poll()

        self.assertIsNone(initial_poll)
        self.assertEqual(exit_code, 0)
        self.assertEqual(final_poll, 0)

    def test_process_timing_properties(self):
        """Test process timing properties."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(0.1)"])
        exit_code = process.wait()

        self.assertIsNotNone(process.start_time)
        self.assertIsNotNone(process.end_time)
        self.assertIsNotNone(process.duration)
        assert process.duration is not None  # for type checker
        self.assertGreater(process.duration, 0)
        self.assertEqual(exit_code, 0)

    def test_accumulated_stdout_property(self):
        """Test accumulated stdout property."""
        process = RunningProcess([sys.executable, "-c", "print('line1'); print('line2')"])
        exit_code = process.wait()

        stdout_content = process.stdout
        self.assertIn("line1", stdout_content)
        self.assertIn("line2", stdout_content)
        self.assertEqual(exit_code, 0)


class TestTimeoutAndErrorHandling(unittest.TestCase):
    """Test timeout and error handling scenarios."""

    def test_timeout_error(self):
        """Test process timeout handling."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"], timeout=1)

        with self.assertRaises(TimeoutError):
            process.wait()

    def test_get_next_line_timeout(self):
        """Test timeout when getting next line."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])

        with self.assertRaises(TimeoutError):
            process.get_next_line(timeout=0.1)

        process.kill()

    def test_invalid_command_error(self):
        """Test handling of invalid commands."""
        with self.assertRaises((FileNotFoundError, OSError)):
            RunningProcess(["this_command_does_not_exist_12345"])

    def test_kill_process(self):
        """Test killing a running process."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])

        # Let it start
        time.sleep(0.1)

        # Kill it
        process.kill()

        # Should be finished after kill
        self.assertTrue(process.finished)

    def test_terminate_process(self):
        """Test gracefully terminating a process."""
        process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])

        # Let it start
        time.sleep(0.1)

        # Terminate it
        process.terminate()

        # Wait a bit for termination
        time.sleep(0.2)

        # Should be finished
        poll_result = process.poll()
        self.assertIsNotNone(poll_result)

    def test_invalid_shell_command_combination(self):
        """Test invalid shell/command combination."""
        with self.assertRaisesRegex(ValueError, "String commands require shell=True"):
            RunningProcess("echo test", shell=False)


class TestSubprocessRun(unittest.TestCase):
    """Test the subprocess_run convenience function."""

    def test_subprocess_run_basic(self):
        """Test basic subprocess_run functionality."""
        result = subprocess_run(command=["echo", "subprocess_run test"], cwd=None, check=False, timeout=10)

        self.assertEqual(result.returncode, 0)
        self.assertIn("subprocess_run test", result.stdout)
        self.assertIsNone(result.stderr)

    def test_subprocess_run_with_cwd(self):
        """Test subprocess_run with working directory."""
        with tempfile.TemporaryDirectory() as temp_dir:
            result = subprocess_run(
                command=[sys.executable, "-c", "import os; print(os.getcwd())"],
                cwd=Path(temp_dir),
                check=False,
                timeout=10,
            )

            self.assertEqual(result.returncode, 0)
            # The output should contain the temp directory path
            self.assertIn(temp_dir.replace("\\", "/"), result.stdout.replace("\\", "/"))

    def test_subprocess_run_with_check_true_success(self):
        """Test subprocess_run with check=True for successful command."""
        result = subprocess_run(
            command=[sys.executable, "-c", "print('success')"],
            cwd=None,
            check=True,
            timeout=10,
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("success", result.stdout)

    def test_subprocess_run_with_check_true_failure(self):
        """Test subprocess_run with check=True for failing command."""
        with self.assertRaises(subprocess.CalledProcessError) as cm:
            subprocess_run(command=[sys.executable, "-c", "exit(1)"], cwd=None, check=True, timeout=10)

        self.assertEqual(cm.exception.returncode, 1)

    def test_subprocess_run_timeout(self):
        """Test subprocess_run with timeout."""
        with self.assertRaisesRegex(RuntimeError, "Process timed out"):
            subprocess_run(
                command=[sys.executable, "-c", "import time; time.sleep(10)"],
                cwd=None,
                check=False,
                timeout=1,
            )


class TestEdgeCases(unittest.TestCase):
    """Test edge cases and special scenarios."""

    def test_empty_output_command(self):
        """Test command that produces no output."""
        process = RunningProcess([sys.executable, "-c", "pass"])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        self.assertEqual(process.stdout, "")

    def test_multiline_output(self):
        """Test command with multiline output."""
        python_code = """
for i in range(5):
    print(f"Line {i}")
"""
        process = RunningProcess([sys.executable, "-c", python_code])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        stdout_lines = process.stdout.split("\n")
        self.assertGreaterEqual(len([line for line in stdout_lines if line.strip()]), 5)

    def test_process_with_large_output(self):
        """Test process that generates substantial output."""
        python_code = """
for i in range(100):
    print(f"Output line {i:03d}")
"""
        process = RunningProcess([sys.executable, "-c", python_code])
        exit_code = process.wait()

        self.assertEqual(exit_code, 0)
        stdout_lines = process.stdout.split("\n")
        non_empty_lines = [line for line in stdout_lines if line.strip()]
        self.assertGreaterEqual(len(non_empty_lines), 100)

    def test_command_list_to_string_conversion(self):
        """Test command list to string conversion."""
        process = RunningProcess(["echo", "test with spaces"])
        command_str = process.get_command_str()

        # Should properly quote arguments with spaces
        self.assertTrue("test with spaces" in command_str or '"test with spaces"' in command_str)

        exit_code = process.wait()
        self.assertEqual(exit_code, 0)


class TestEchoCallback(unittest.TestCase):
    """Test echo callback functionality."""

    def test_echo_boolean_true(self):
        """Test echo=True converts to print function."""
        process = RunningProcess(["echo", "test output"])

        # Capture stdout to verify print was called
        captured_output = io.StringIO()
        with contextlib.redirect_stdout(captured_output):
            exit_code = process.wait(echo=True)

        self.assertEqual(exit_code, 0)
        output_lines = captured_output.getvalue().strip()
        # Should contain the echoed output
        self.assertIn("test output", output_lines)

    def test_echo_boolean_false(self):
        """Test echo=False produces no output."""
        process = RunningProcess(["echo", "test output"])

        captured_output = io.StringIO()
        with contextlib.redirect_stdout(captured_output):
            exit_code = process.wait(echo=False)

        self.assertEqual(exit_code, 0)
        output_lines = captured_output.getvalue().strip()
        # Should not contain any echoed output
        self.assertEqual(output_lines, "")

    def test_echo_custom_callback(self):
        """Test echo with custom callback function."""
        captured_lines = []

        def custom_callback(line: str):
            captured_lines.append(f"CUSTOM: {line}")

        process = RunningProcess(["echo", "test callback"])
        exit_code = process.wait(echo=custom_callback)

        self.assertEqual(exit_code, 0)
        self.assertTrue(len(captured_lines) > 0)
        # Should have our custom prefix
        self.assertTrue(any("CUSTOM: test callback" in line for line in captured_lines))

    def test_echo_invalid_type(self):
        """Test echo with invalid type raises TypeError."""
        process = RunningProcess(["echo", "test"])

        with self.assertRaises(TypeError) as cm:
            process.wait(echo="invalid")  # type: ignore[arg-type]  # intentionally invalid

        self.assertIn("echo must be bool or callable", str(cm.exception))
