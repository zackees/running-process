#!/usr/bin/env python3
"""Process utilities for managing processes and process trees."""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import time
import warnings

try:
    import psutil
except ImportError:
    psutil = None
    warnings.warn(
        "psutil not installed. Process tree operations will be limited.",
        UserWarning,
        stacklevel=2,
    )


def get_process_tree_info(pid: int) -> str:
    """Get information about a process and its children."""
    if psutil is None:
        return f"psutil not available - cannot get process info for PID {pid}"

    try:
        process = psutil.Process(pid)
        info = [f"Process {pid} ({process.name()})"]
        info.append(f"Status: {process.status()}")
        info.append(f"CPU Times: {process.cpu_times()}")
        info.append(f"Memory: {process.memory_info()}")

        # Get child processes
        children = process.children(recursive=True)
        if children:
            info.append("\nChild processes:")
            for child in children:
                info.append(f"  Child {child.pid} ({child.name()})")
                info.append(f"    Status: {child.status()}")
                info.append(f"    CPU Times: {child.cpu_times()}")
                info.append(f"    Memory: {child.memory_info()}")

        return "\n".join(info)
    except Exception:  # noqa: BLE001
        return f"Could not get process info for PID {pid}"


def _kill_process_fallback(pid: int) -> None:
    """Fallback process killing when psutil is not available."""
    with contextlib.suppress(OSError, subprocess.SubprocessError):
        if os.name == "nt":
            # Use subprocess.run with check for better security
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], check=False)  # noqa: S607, S603
        else:
            os.kill(pid, signal.SIGTERM)
            time.sleep(0.5)
            os.kill(pid, signal.SIGKILL)


def kill_process_tree(pid: int) -> None:
    """Kill a process and all its children."""
    if psutil is None:
        _kill_process_fallback(pid)
        return

    try:
        parent = psutil.Process(pid)
        children = parent.children(recursive=True)

        # First try graceful termination
        for child in children:
            with contextlib.suppress(psutil.NoSuchProcess):
                child.terminate()

        # Give them a moment to terminate
        _, alive = psutil.wait_procs(children, timeout=3)

        # Force kill any that are still alive
        for child in alive:
            with contextlib.suppress(psutil.NoSuchProcess):
                child.kill()

        # Finally terminate the parent
        with contextlib.suppress(psutil.NoSuchProcess, psutil.TimeoutExpired):
            parent.terminate()
            parent.wait(3)  # Give it 3 seconds to terminate

        with contextlib.suppress(psutil.NoSuchProcess):
            parent.kill()  # Force kill if still alive

    except (OSError, psutil.Error) as e:
        warnings.warn(f"Error killing process tree: {e}", UserWarning, stacklevel=2)
