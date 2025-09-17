"""Process watcher module.

This module contains the ProcessWatcher class for monitoring subprocess execution
in a background thread.
"""

import _thread
import contextlib
import logging
import subprocess
import threading
import time
import traceback
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from running_process.running_process import RunningProcess

logger = logging.getLogger(__name__)


class ProcessWatcher:
    """Background watcher that polls a process until it terminates."""

    def __init__(self, running_process: "RunningProcess") -> None:
        self._rp = running_process
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        name: str = "RPWatcher"
        with contextlib.suppress(AttributeError, TypeError):
            if self._rp.proc is not None:
                name = f"RPWatcher-{self._rp.proc.pid}"

        self._thread = threading.Thread(target=self._run, name=name, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        thread_id = threading.current_thread().ident
        thread_name = threading.current_thread().name
        try:
            while not self._rp.shutdown.is_set():
                # Enforce per-process timeout independently of wait()
                if (
                    self._rp.timeout is not None
                    and self._rp.start_time is not None
                    and (time.time() - self._rp.start_time) > self._rp.timeout
                ):
                    logger.warning(
                        "Process timeout after %s seconds (watcher), killing: %s",
                        self._rp.timeout,
                        self._rp.command,
                    )
                    # Execute user-provided timeout callback if available
                    if self._rp.on_timeout is not None:
                        try:
                            process_info = self._rp._create_process_info()  # noqa: SLF001
                            self._rp.on_timeout(process_info)
                        except (AttributeError, TypeError, ValueError, RuntimeError) as e:
                            logger.warning("Watcher timeout callback failed: %s", e)
                    self._rp.kill()
                    break

                rc: int | None = self._rp.poll()
                if rc is not None:
                    break
                time.sleep(0.1)
        except KeyboardInterrupt:
            logger.warning("Thread %s (%s) caught KeyboardInterrupt", thread_id, thread_name)
            logger.warning("Stack trace for thread %s:", thread_id)
            traceback.print_exc()
            _thread.interrupt_main()
            raise
        except (OSError, subprocess.SubprocessError, RuntimeError) as e:
            # Surface unexpected errors and keep behavior consistent
            logger.warning("Watcher thread error in %s: %s", thread_name, e)
            traceback.print_exc()

    @property
    def thread(self) -> threading.Thread | None:
        return self._thread
