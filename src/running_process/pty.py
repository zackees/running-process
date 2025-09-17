"""Cross-platform PTY (Pseudo-Terminal) wrapper.

This module provides a unified interface for pseudo-terminal functionality
across different platforms (Windows, Unix/Linux/macOS).
"""

import os
import signal
import subprocess
import sys
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from typing import Protocol

    class PtyProcessProtocol(Protocol):
        """Protocol for PTY process implementations."""

        pid: int
        returncode: int | None
        stdout: Any  # PTY processes handle stdout differently

        def poll(self) -> int | None: ...
        def kill(self) -> None: ...
        def terminate(self) -> None: ...
        def wait(self, timeout: float | None = None) -> int | None: ...
        def read(self, size: int = 1024) -> bytes: ...
        def write(self, data: bytes) -> int: ...
        def close(self) -> None: ...


class PtyNotAvailableError(Exception):
    """Raised when PTY functionality is not available on the current platform."""


class Pty:
    """Unified cross-platform PTY wrapper.

    This class provides a consistent interface for pseudo-terminal operations
    across Windows (using winpty) and Unix-like systems (using pty module).
    """

    def __init__(self) -> None:
        self._pty_proc: Any = None
        self._master_fd: int | None = None
        self._slave_fd: int | None = None
        self._platform = sys.platform

    @classmethod
    def is_available(cls) -> bool:
        """Check if PTY support is available on the current platform."""
        if sys.platform == "win32":
            try:
                import winpty  # noqa: F401, PLC0415  # type: ignore[import-untyped,import-not-found]
            except ImportError:
                return False
            else:
                return True
        else:
            try:
                import pty  # noqa: F401, PLC0415
            except ImportError:
                return False
            else:
                return True

    def spawn_process(
        self,
        command: str | list[str],
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        shell: bool = False,
    ) -> "PtyProcessProtocol":
        """Spawn a process with PTY support.

        Args:
            command: Command to execute (string or list)
            cwd: Working directory for the process
            env: Environment variables
            shell: Whether to use shell for execution

        Returns:
            A process object with PTY support

        Raises:
            PtyNotAvailableError: If PTY is not available on this platform
        """
        if not self.is_available():
            msg = f"PTY not available on {self._platform}"
            raise PtyNotAvailableError(msg)

        if self._platform == "win32":
            return self._spawn_windows_process(command, cwd, env, shell)
        return self._spawn_unix_process(command, cwd, env, shell)

    def _spawn_windows_process(
        self,
        command: str | list[str],
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        shell: bool = False,
    ) -> "WindowsPtyProcess":
        """Spawn a Windows PTY process using winpty."""
        import winpty  # noqa: PLC0415  # type: ignore[import-untyped,import-not-found]

        # Prepare command for winpty
        pty_command = (["cmd", "/c", command] if shell else command.split()) if isinstance(command, str) else command

        # Use current environment if none provided
        if env is None:
            env = os.environ.copy()

        # Create PTY process
        self._pty_proc = winpty.PtyProcess.spawn(
            pty_command,
            cwd=cwd,
            env=env,
        )

        return WindowsPtyProcess(self._pty_proc)

    def _spawn_unix_process(
        self,
        command: str | list[str],
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        shell: bool = False,
    ) -> "UnixPtyProcess":
        """Spawn a Unix PTY process using pty module."""
        import pty  # noqa: PLC0415

        # Create PTY master and slave
        master_fd, slave_fd = pty.openpty()  # type: ignore[attr-defined]
        self._master_fd = master_fd
        self._slave_fd = slave_fd

        # Prepare command - both list and str are handled the same way for Unix
        popen_command = command

        # Create process with PTY
        proc = subprocess.Popen(  # noqa: S603
            popen_command,
            shell=shell,
            cwd=cwd,
            env=env,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,  # All streams use PTY
            text=False,  # Use binary mode for PTY
            preexec_fn=os.setsid if sys.platform != "win32" else None,  # noqa: PLW1509
        )

        # Close slave fd in parent process
        os.close(slave_fd)
        self._slave_fd = None

        return UnixPtyProcess(proc, master_fd)


class WindowsPtyProcess:
    """Windows PTY process wrapper using winpty."""

    def __init__(self, pty_proc: Any) -> None:
        self._pty = pty_proc
        self.pid = pty_proc.pid
        self.returncode: int | None = None
        self.stdout = None  # PTY handles output differently

    def poll(self) -> int | None:
        """Check if process has terminated."""
        if self._pty.isalive():
            return None
        if self.returncode is None:
            self.returncode = self._pty.exitstatus
        return self.returncode

    def terminate(self) -> None:
        """Terminate the process gracefully."""
        self._pty.terminate()

    def kill(self) -> None:
        """Forcefully kill the process."""
        self._pty.kill(signal.SIGTERM)

    def wait(self, timeout: float | None = None) -> int | None:
        """Wait for process to complete."""
        self._pty.wait(timeout)
        return self.poll()

    def read(self, size: int = 1024) -> bytes:
        """Read data from PTY."""
        return self._pty.read(size)

    def write(self, data: bytes) -> int:
        """Write data to PTY."""
        return self._pty.write(data)

    def close(self) -> None:
        """Close PTY resources."""
        if hasattr(self._pty, "close"):
            self._pty.close()


class UnixPtyProcess:
    """Unix PTY process wrapper using pty module."""

    def __init__(self, proc: subprocess.Popen[Any], master_fd: int) -> None:
        self._proc = proc
        self._master_fd = master_fd
        self.pid = proc.pid
        self.returncode: int | None = None
        self.stdout = None  # PTY handles output differently

    def poll(self) -> int | None:
        """Check if process has terminated."""
        result = self._proc.poll()
        if result is not None:
            self.returncode = result
        return result

    def terminate(self) -> None:
        """Terminate the process gracefully."""
        self._proc.terminate()

    def kill(self) -> None:
        """Forcefully kill the process."""
        self._proc.kill()

    def wait(self, timeout: float | None = None) -> int | None:
        """Wait for process to complete."""
        result = self._proc.wait(timeout)
        self.returncode = result
        return result

    def read(self, size: int = 1024) -> bytes:
        """Read data from PTY master."""
        if self._master_fd is None:
            msg = "PTY master fd is closed"
            raise ValueError(msg)
        return os.read(self._master_fd, size)

    def write(self, data: bytes) -> int:
        """Write data to PTY master."""
        if self._master_fd is None:
            msg = "PTY master fd is closed"
            raise ValueError(msg)
        return os.write(self._master_fd, data)

    def close(self) -> None:
        """Close PTY resources."""
        if self._master_fd is not None:
            os.close(self._master_fd)
            self._master_fd = None
