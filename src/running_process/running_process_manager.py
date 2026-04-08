from __future__ import annotations

import contextlib
import threading
import time
import warnings
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from running_process.running_process import RunningProcess


class RunningProcessManager:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._processes: list[RunningProcess] = []

    def register(self, proc: RunningProcess) -> None:
        with self._lock:
            if proc not in self._processes:
                self._processes.append(proc)

    def unregister(self, proc: RunningProcess) -> None:
        with self._lock, contextlib.suppress(ValueError):
            self._processes.remove(proc)

    def list_active(self) -> list[RunningProcess]:
        with self._lock:
            return [proc for proc in self._processes if not proc.finished]

    def dump_active(self) -> None:
        active = self.list_active()
        if not active:
            warnings.warn("NO ACTIVE SUBPROCESSES DETECTED", UserWarning, stacklevel=2)
            return

        warnings.warn("STUCK SUBPROCESS COMMANDS:", UserWarning, stacklevel=2)
        now = time.time()
        for index, proc in enumerate(active, start=1):
            duration = "?"
            if proc.start_time is not None:
                duration = f"{now - proc.start_time:.1f}s"
            warnings.warn(
                f"  {index}. cmd={proc.command} pid={proc.pid} duration={duration}",
                UserWarning,
                stacklevel=2,
            )


RunningProcessManagerSingleton = RunningProcessManager()
