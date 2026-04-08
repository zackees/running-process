from __future__ import annotations

import contextlib

import psutil


def get_process_tree_info(pid: int) -> str:
    try:
        process = psutil.Process(pid)
        info = [f"Process {pid} ({process.name()})", f"Status: {process.status()}"]
        children = process.children(recursive=True)
        if children:
            info.append("Child processes:")
            for child in children:
                info.append(f"  Child {child.pid} ({child.name()})")
        return "\n".join(info)
    except Exception:
        return f"Could not get process info for PID {pid}"


def kill_process_tree(pid: int) -> None:
    try:
        parent = psutil.Process(pid)
        children = parent.children(recursive=True)
        for child in children:
            with contextlib.suppress(psutil.NoSuchProcess):
                child.kill()
        with contextlib.suppress(psutil.NoSuchProcess):
            parent.kill()
    except psutil.Error:
        return None
