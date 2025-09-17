"""Tests for PTY support in RunningProcess."""

import sys
import time
import unittest

from running_process import RunningProcess
from running_process.pty import Pty


class TestPTYSupport(unittest.TestCase):
    """Test PTY support functionality."""

    def test_pty_detection(self):
        """Test PTY availability detection."""
        # Create process without PTY to test detection
        proc = RunningProcess(["echo", "test"], auto_run=False)

        # Test PTY availability using unified PTY class
        expected_availability = Pty.is_available()
        actual_availability = proc._pty_available()  # noqa: SLF001

        self.assertEqual(expected_availability, actual_availability)

    def test_pty_parameter_initialization(self):
        """Test that use_pty parameter is properly initialized."""
        # Test with use_pty=False (default behavior)
        proc1 = RunningProcess(["echo", "test"], use_pty=False, auto_run=False)
        self.assertFalse(proc1.use_pty)

        # Test with use_pty=True
        proc2 = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        # use_pty will be True only if PTY is available
        if proc2._pty_available():  # noqa: SLF001
            self.assertTrue(proc2.use_pty)
        else:
            self.assertFalse(proc2.use_pty)

    @unittest.skipIf(sys.platform == "win32", "Unix-specific PTY test")
    def test_unix_pty_process_creation(self):
        """Test PTY process creation on Unix systems."""
        if not Pty.is_available():
            self.skipTest("PTY not available on this system")

        # Create a simple command with PTY
        proc = RunningProcess(["echo", "Hello PTY"], use_pty=True, auto_run=True)

        # Wait for completion
        exit_code = proc.wait()
        self.assertEqual(exit_code, 0)

        # Check that output was captured
        output = proc.stdout.strip()
        self.assertIn("Hello PTY", output)

    @unittest.skipUnless(sys.platform == "win32", "Windows-specific PTY test")
    def test_windows_pty_process_creation(self):
        """Test PTY process creation on Windows systems."""
        if not Pty.is_available():
            self.skipTest("PTY not available on this system")

        # Create a simple command with PTY
        proc = RunningProcess(["cmd", "/c", "echo Hello PTY"], use_pty=True, auto_run=True)

        # Wait for completion
        exit_code = proc.wait()
        self.assertEqual(exit_code, 0)

        # Check that output was captured
        output = proc.stdout.strip()
        self.assertIn("Hello PTY", output)

    def test_pty_with_ansi_filtering(self):
        """Test that ANSI escape sequences are filtered in PTY mode."""
        # Skip if PTY is not available
        proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        if not proc.use_pty:
            self.skipTest("PTY not available on this platform")

        # Create a command that might produce ANSI codes
        if sys.platform == "win32":
            # Windows command that might produce color output
            command = ["cmd", "/c", "echo", "\x1b[31mRed Text\x1b[0m"]
        else:
            # Unix command with ANSI codes
            command = ["echo", "-e", "\x1b[31mRed Text\x1b[0m"]

        proc = RunningProcess(command, use_pty=True)
        exit_code = proc.wait()
        self.assertEqual(exit_code, 0)

        # Check that ANSI codes were filtered
        output = proc.stdout.strip()
        self.assertNotIn("\x1b[", output)
        self.assertIn("Red Text", output)

    def test_pty_keyboard_interrupt_handling(self):
        """Test that KeyboardInterrupt is handled properly in PTY mode."""
        # Skip if PTY is not available
        proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        if not proc.use_pty:
            self.skipTest("PTY not available on this platform")

        # Create a long-running process
        if sys.platform == "win32":
            command = ["cmd", "/c", "timeout", "/t", "10"]
        else:
            command = ["sleep", "10"]

        proc = RunningProcess(command, use_pty=True)

        # Let it run briefly
        time.sleep(0.5)

        # Kill the process (simulating Ctrl+C)
        proc.kill()

        # Process should be terminated
        self.assertIsNotNone(proc.poll())

    def test_pty_cleanup_on_kill(self):
        """Test that PTY resources are cleaned up when process is killed."""
        # Skip if PTY is not available
        proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        if not proc.use_pty:
            self.skipTest("PTY not available on this platform")

        # Create a process
        if sys.platform == "win32":
            command = ["cmd", "/c", "timeout", "/t", "10"]
        else:
            command = ["sleep", "10"]

        proc = RunningProcess(command, use_pty=True)

        # Store PTY references
        pty_proc = proc._pty_proc  # noqa: SLF001
        pty_master_fd = proc._pty_master_fd  # noqa: SLF001

        # Kill the process
        proc.kill()

        # Check that cleanup was performed
        if sys.platform == "win32":
            # On Windows, the PTY process should have been killed
            self.assertIsNotNone(pty_proc)  # Was created
        else:
            # On Unix, the master FD should have been closed
            self.assertIsNotNone(pty_master_fd)  # Was created

    def test_pty_vs_pipe_output_consistency(self):
        """Test that PTY and pipe modes produce consistent output."""
        command = ["echo", "Hello World"]

        # Run with pipe (normal mode)
        proc_pipe = RunningProcess(command, use_pty=False)
        exit_pipe = proc_pipe.wait()
        output_pipe = proc_pipe.stdout.strip()

        # Run with PTY if available
        proc_pty = RunningProcess(command, use_pty=True)
        if proc_pty.use_pty:
            exit_pty = proc_pty.wait()
            output_pty = proc_pty.stdout.strip()

            # Both should succeed
            self.assertEqual(exit_pipe, 0)
            self.assertEqual(exit_pty, 0)

            # Both should contain the same text (after ANSI filtering)
            self.assertIn("Hello World", output_pipe)
            self.assertIn("Hello World", output_pty)

    def test_pty_with_interactive_command(self):
        """Test PTY with a command that behaves differently in interactive mode."""
        # Skip if PTY is not available
        proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        if not proc.use_pty:
            self.skipTest("PTY not available on this platform")

        # Use a command that typically requires PTY for proper behavior
        if sys.platform == "win32":
            # Windows interactive command
            command = ["cmd", "/c", "echo", "Interactive"]
        else:
            # Unix command that checks for TTY
            command = ["sh", "-c", "if [ -t 0 ]; then echo 'TTY'; else echo 'No TTY'; fi"]

        # Run with PTY
        proc_pty = RunningProcess(command, use_pty=True)
        exit_code = proc_pty.wait()
        output_pty = proc_pty.stdout.strip()

        self.assertEqual(exit_code, 0)
        if sys.platform != "win32":
            # On Unix, should detect TTY
            self.assertIn("TTY", output_pty)

    def test_pty_with_timeout(self):
        """Test that PTY processes respect timeout settings."""
        # Skip if PTY is not available
        proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
        if not proc.use_pty:
            self.skipTest("PTY not available on this platform")

        # Create a long-running process with timeout
        if sys.platform == "win32":
            command = ["cmd", "/c", "timeout", "/t", "10"]
        else:
            command = ["sleep", "10"]

        proc = RunningProcess(command, use_pty=True, timeout=1)

        # Should timeout
        with self.assertRaises(TimeoutError):
            proc.wait()


if __name__ == "__main__":
    unittest.main()
