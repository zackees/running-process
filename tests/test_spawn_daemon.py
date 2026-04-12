"""Tests for the spawn_daemon() API and DaemonHandle.

Non-live tests run in CI.  The Windows console popup test is @live only.
"""

import os
import sys
import time
import unittest

import pytest

live = pytest.mark.live
is_windows = sys.platform == "win32"
skip_unless_windows = pytest.mark.skipif(not is_windows, reason="Windows-only test")


def _trampoline_available() -> bool:
    """Return True if the bundled trampoline binary exists."""
    try:
        from running_process.daemon import _bundled_trampoline_path

        return _bundled_trampoline_path().exists()
    except Exception:
        return False


requires_trampoline = pytest.mark.skipif(
    not _trampoline_available(),
    reason="Trampoline binary not bundled in this build",
)


@requires_trampoline
class TestSpawnDaemon(unittest.TestCase):
    """Core spawn_daemon tests — run on all platforms in CI."""

    _daemon_name: str = ""

    def _unique_name(self, label: str) -> str:
        self._daemon_name = f"test-daemon-{label}-{os.getpid()}"
        return self._daemon_name

    def tearDown(self) -> None:
        if self._daemon_name:
            from running_process.daemon import cleanup_runtime

            cleanup_runtime(self._daemon_name)
            self._daemon_name = ""

    def test_spawn_returns_handle(self):
        """spawn_daemon returns a DaemonHandle with pid, name, runtime_dir."""
        from running_process.daemon import DaemonHandle, spawn_daemon

        name = self._unique_name("handle")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
        )
        try:
            self.assertIsInstance(handle, DaemonHandle)
            self.assertIsInstance(handle.pid, int)
            self.assertGreater(handle.pid, 0)
            self.assertEqual(handle.name, name)
            self.assertTrue(handle.runtime_dir.exists())
        finally:
            self._kill_pid(handle.pid)

    def test_pid_file_written(self):
        """spawn_daemon writes a daemon.pid file in the runtime directory."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("pidfile")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
        )
        try:
            pid_file = handle.runtime_dir / "daemon.pid"
            self.assertTrue(pid_file.exists(), "daemon.pid not found")
            self.assertEqual(pid_file.read_text().strip(), str(handle.pid))
        finally:
            self._kill_pid(handle.pid)

    def test_process_is_running(self):
        """is_running() returns True while the daemon is alive."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("running")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(10)"],
            name=name,
        )
        try:
            self.assertTrue(handle.is_running())
        finally:
            self._kill_pid(handle.pid)

    def test_log_file_receives_output(self):
        """When log_path is set, daemon stdout goes to the file."""
        from running_process.daemon import runtime_dir, spawn_daemon

        name = self._unique_name("logfile")
        log_path = runtime_dir(name) / "test.log"

        handle = spawn_daemon(
            [sys.executable, "-c", "print('HELLO_DAEMON'); import time; time.sleep(2)"],
            name=name,
            log_path=log_path,
        )
        try:
            # Give the daemon time to start and write output.
            deadline = time.monotonic() + 10
            found = False
            while time.monotonic() < deadline:
                time.sleep(0.3)
                if log_path.exists():
                    content = log_path.read_text(encoding="utf-8", errors="replace")
                    if "HELLO_DAEMON" in content:
                        found = True
                        break
            self.assertTrue(found, f"Expected 'HELLO_DAEMON' in {log_path}")
        finally:
            self._kill_pid(handle.pid)

    def test_daemon_exits_cleanly(self):
        """A daemon whose command exits quickly stops running."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("exitclean")
        handle = spawn_daemon(
            [sys.executable, "-c", "print('done')"],
            name=name,
        )
        # Wait for the daemon to exit.
        deadline = time.monotonic() + 10
        while handle.is_running() and time.monotonic() < deadline:
            time.sleep(0.2)
        self.assertFalse(handle.is_running(), "Daemon should have exited")

    def test_read_stdout_raises(self):
        """DaemonHandle.read_stdout() raises DaemonOutputNotAvailableError."""
        from running_process.daemon import DaemonOutputNotAvailableError, spawn_daemon

        name = self._unique_name("readstdout")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
        )
        try:
            with self.assertRaises(DaemonOutputNotAvailableError):
                handle.read_stdout()
        finally:
            self._kill_pid(handle.pid)

    def test_cwd_is_passed(self):
        """spawn_daemon forwards cwd to the sidecar JSON."""
        import json
        from pathlib import Path

        from running_process.daemon import spawn_daemon

        name = self._unique_name("cwd")
        cwd_path = Path.home()
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
            cwd=cwd_path,
        )
        try:
            sidecar = handle.runtime_dir / f"{name}.daemon.json"
            data = json.loads(sidecar.read_text(encoding="utf-8"))
            self.assertEqual(data["cwd"], str(cwd_path))
        finally:
            self._kill_pid(handle.pid)

    def test_env_is_passed(self):
        """spawn_daemon forwards env to the sidecar JSON."""
        import json

        from running_process.daemon import spawn_daemon

        name = self._unique_name("env")
        test_env = {"MY_VAR": "hello", "OTHER": "world"}
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
            env=test_env,
        )
        try:
            sidecar = handle.runtime_dir / f"{name}.daemon.json"
            data = json.loads(sidecar.read_text(encoding="utf-8"))
            self.assertEqual(data["env"], test_env)
        finally:
            self._kill_pid(handle.pid)

    @staticmethod
    def _kill_pid(pid: int) -> None:
        """Best-effort kill of a process by PID."""
        try:
            if sys.platform == "win32":
                import subprocess

                subprocess.run(
                    ["taskkill", "/F", "/PID", str(pid)],
                    capture_output=True,
                    check=False,
                )
            else:
                import signal

                os.kill(pid, signal.SIGKILL)
        except OSError:
            pass


@live
@skip_unless_windows
@requires_trampoline
class TestSpawnDaemonNoPopup(unittest.TestCase):
    """Verify spawn_daemon does not flash a console window on Windows."""

    def test_no_console_popup(self):
        """spawn_daemon should not create any visible console windows."""
        from running_process.daemon import cleanup_runtime, spawn_daemon
        from tests.test_console_detection import assert_no_console_popup

        name = f"test-popup-{os.getpid()}"
        try:
            with assert_no_console_popup(duration_secs=4.0):
                handle = spawn_daemon(
                    [sys.executable, "-c", "import time; time.sleep(3)"],
                    name=name,
                )
                time.sleep(3.0)
                TestSpawnDaemon._kill_pid(handle.pid)
        finally:
            cleanup_runtime(name)
