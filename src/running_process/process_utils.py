from __future__ import annotations

import sys
import time
from contextlib import suppress

import psutil

from running_process._native import native_get_process_tree_info, native_kill_process_tree


def get_process_tree_info(pid: int) -> str:
    return native_get_process_tree_info(int(pid))


def _wait_for_pids_gone(pids: set[int], timeout_seconds: float) -> None:
    deadline = time.monotonic() + max(0.0, timeout_seconds)
    while pids:
        pids = {pid for pid in pids if psutil.pid_exists(pid)}
        if not pids or time.monotonic() >= deadline:
            return
        time.sleep(0.025)


def kill_process_tree(pid: int, timeout_seconds: float = 3.0) -> None:
    pid = int(pid)
    if sys.platform == "win32":
        native_kill_process_tree(pid, timeout_seconds)
        return

    try:
        parent = psutil.Process(pid)
    except psutil.Error:
        native_kill_process_tree(pid, timeout_seconds)
        return

    descendants = parent.children(recursive=True)
    targets = [*reversed(descendants), parent]
    target_pids = {process.pid for process in targets}

    for process in targets:
        with suppress(psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            process.kill()

    _wait_for_pids_gone(set(target_pids), timeout_seconds)
    remaining = {target_pid for target_pid in target_pids if psutil.pid_exists(target_pid)}
    if remaining:
        native_kill_process_tree(pid, timeout_seconds)
        _wait_for_pids_gone(remaining, timeout_seconds)
