"""Running process manager for tracking active processes."""

from __future__ import annotations

import contextlib
import threading
import time
import warnings
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from running_process.running_process import RunningProcess


class RunningProcessManager:
    """Thread-safe registry of currently running processes for diagnostics."""

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._processes: list[RunningProcess] = []

    def register(self, proc: RunningProcess) -> None:
        """Register a running process."""
        with self._lock:
            if proc not in self._processes:
                self._processes.append(proc)

    def unregister(self, proc: RunningProcess) -> None:
        """Unregister a process."""
        with self._lock, contextlib.suppress(ValueError):
            self._processes.remove(proc)

    def list_active(self) -> list[RunningProcess]:
        """List all active processes."""
        with self._lock:
            return [p for p in self._processes if not p.finished]

    def dump_active(self) -> None:
        """Dump information about active processes."""
        active: list[RunningProcess] = self.list_active()
        if not active:
            warnings.warn(
                "NO ACTIVE SUBPROCESSES DETECTED - MAIN PROCESS LIKELY HUNG",
                UserWarning,
                stacklevel=2,
            )
            return

        warnings.warn("STUCK SUBPROCESS COMMANDS:", UserWarning, stacklevel=2)

        now = time.time()
        for idx, p in enumerate(active, 1):
            pid: int | None = None
            try:
                if p.proc is not None:
                    pid = p.proc.pid
            except Exception:  # noqa: BLE001
                pid = None

            start = p.start_time
            last_out = p.time_last_stdout_line()
            duration_str = f"{(now - start):.1f}s" if start is not None else "?"
            since_out_str = f"{(now - last_out):.1f}s" if last_out is not None else "no-output"

            warnings.warn(
                f"  {idx}. cmd={p.command} pid={pid} duration={duration_str} last_output={since_out_str}",
                UserWarning,
                stacklevel=2,
            )


# Global singleton instance for convenient access
RunningProcessManagerSingleton = RunningProcessManager()
