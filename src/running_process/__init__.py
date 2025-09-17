"""A modern subprocess.Popen wrapper with improved process management."""

from __future__ import annotations

__version__ = "1.0.0"

from running_process.output_formatter import OutputFormatter, TimeDeltaFormatter
from running_process.process_utils import get_process_tree_info, kill_process_tree
from running_process.running_process import RunningProcess
from running_process.running_process_manager import RunningProcessManager, RunningProcessManagerSingleton

__all__ = [
    "OutputFormatter",
    "RunningProcess",
    "RunningProcessManager",
    "RunningProcessManagerSingleton",
    "TimeDeltaFormatter",
    "get_process_tree_info",
    "kill_process_tree",
]
