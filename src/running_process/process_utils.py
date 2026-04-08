from __future__ import annotations

from running_process._native import (
    native_get_process_tree_info,
    native_kill_process_tree,
)


def get_process_tree_info(pid: int) -> str:
    return native_get_process_tree_info(int(pid))


def kill_process_tree(pid: int) -> None:
    native_kill_process_tree(int(pid))
