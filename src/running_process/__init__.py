"""A modern subprocess.Popen wrapper with improved process management."""

from __future__ import annotations

__version__ = "0.0.1"

from running_process.output_formatter import OutputFormatter
from running_process.running_process import RunningProcess
from running_process.running_process_manager import RunningProcessManager

__all__ = [
    "OutputFormatter",
    "RunningProcess",
    "RunningProcessManager",
]
