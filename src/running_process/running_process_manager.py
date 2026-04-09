from __future__ import annotations

import time
import warnings
from dataclasses import dataclass

from running_process._native import native_list_active_processes
from running_process.process_utils import kill_process_tree


@dataclass(frozen=True, slots=True)
class ActiveProcessInfo:
    pid: int
    kind: str
    command: str
    cwd: str | None
    start_time: float

    @property
    def finished(self) -> bool:
        return False

    @property
    def duration(self) -> float:
        return max(0.0, time.time() - self.start_time)

    def kill(self) -> None:
        kill_process_tree(self.pid)


class RunningProcessManager:
    def register(self, _proc: object) -> None:
        return None

    def unregister(self, _proc: object) -> None:
        return None

    def list_active(self) -> list[ActiveProcessInfo]:
        return [ActiveProcessInfo(*row) for row in native_list_active_processes()]

    def dump_active(self) -> None:
        active = self.list_active()
        if not active:
            warnings.warn("NO ACTIVE SUBPROCESSES DETECTED", UserWarning, stacklevel=2)
            return

        warnings.warn("STUCK SUBPROCESS COMMANDS:", UserWarning, stacklevel=2)
        for index, proc in enumerate(active, start=1):
            warnings.warn(
                f"  {index}. cmd={proc.command} pid={proc.pid} duration={proc.duration:.1f}s",
                UserWarning,
                stacklevel=2,
            )


RunningProcessManagerSingleton = RunningProcessManager()
