from __future__ import annotations

__version__ = "3.0.0"

from running_process.exit_status import ExitStatus, ProcessAbnormalExit
from running_process.expect import ExpectMatch, ExpectRule
from running_process.output_formatter import OutputFormatter, TimeDeltaFormatter
from running_process.priority import CpuPriority
from running_process.process_utils import get_process_tree_info, kill_process_tree
from running_process.pty import (
    IdleWaitResult,
    InteractiveLaunchSpec,
    InteractiveMode,
    InteractiveProcess,
    InterruptResult,
    PseudoTerminalProcess,
    PtyNotAvailableError,
)
from running_process.running_process import (
    EndOfStream,
    ProcessInfo,
    RunningProcess,
    subprocess_run,
)
from running_process.running_process_manager import (
    RunningProcessManager,
    RunningProcessManagerSingleton,
)

__all__ = [
    "CpuPriority",
    "EndOfStream",
    "ExitStatus",
    "ExpectMatch",
    "ExpectRule",
    "IdleWaitResult",
    "InteractiveLaunchSpec",
    "InteractiveMode",
    "InteractiveProcess",
    "InterruptResult",
    "OutputFormatter",
    "ProcessAbnormalExit",
    "ProcessInfo",
    "PseudoTerminalProcess",
    "PtyNotAvailableError",
    "RunningProcess",
    "RunningProcessManager",
    "RunningProcessManagerSingleton",
    "TimeDeltaFormatter",
    "get_process_tree_info",
    "kill_process_tree",
    "subprocess_run",
]
