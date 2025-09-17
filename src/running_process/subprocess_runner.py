"""Private subprocess runner module.

This module contains the private implementation of subprocess.run() replacement
using RunningProcess as the backend.
"""

import subprocess
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from running_process.running_process import ProcessInfo


def execute_subprocess_run(
    command: str | list[str],
    cwd: Path | None,
    check: bool,
    timeout: int,
    on_timeout: Callable[["ProcessInfo"], None] | None = None,
) -> subprocess.CompletedProcess[str]:
    """
    Execute a command with robust stdout handling, emulating subprocess.run().

    Uses RunningProcess as the backend to provide:
    - Continuous stdout streaming to prevent pipe blocking
    - Merged stderr into stdout for unified output
    - Timeout protection with optional stack trace dumping
    - Standard subprocess.CompletedProcess return value

    Args:
        command: Command to execute as string or list of arguments.
        cwd: Working directory for command execution. Required parameter.
        check: If True, raise CalledProcessError for non-zero exit codes.
        timeout: Maximum execution time in seconds.
        on_timeout: Callback function executed when process times out.

    Returns:
        CompletedProcess with combined stdout and process return code.
        stderr field is None since it's merged into stdout.

    Raises:
        RuntimeError: If process times out (wraps TimeoutError).
        CalledProcessError: If check=True and process exits with non-zero code.
    """
    # Import here to avoid circular imports during module load
    from running_process.running_process import RunningProcess  # noqa: PLC0415

    # Use RunningProcess for robust stdout pumping with merged stderr
    proc = RunningProcess(
        command=command,
        cwd=cwd,
        check=False,
        auto_run=True,
        timeout=timeout,
        on_timeout=on_timeout,
        on_complete=None,
        output_formatter=None,
    )

    try:
        return_code: int = proc.wait()
    except KeyboardInterrupt:
        # Propagate interrupt behavior consistent with subprocess.run
        raise
    except TimeoutError as e:
        # Align with subprocess.TimeoutExpired semantics by raising a CalledProcessError-like
        # error with available output. Using TimeoutError here is consistent with internal RP.
        error_message = f"CRITICAL: Process timed out after {timeout} seconds: {command}"
        raise RuntimeError(error_message) from e

    combined_stdout: str = proc.stdout

    # Construct CompletedProcess (stderr is merged into stdout by design)
    completed = subprocess.CompletedProcess(
        args=command,
        returncode=return_code,
        stdout=combined_stdout,
        stderr=None,
    )

    if check and return_code != 0:
        # Raise the standard exception with captured output
        raise subprocess.CalledProcessError(
            returncode=return_code,
            cmd=command,
            output=combined_stdout,
            stderr=None,
        )

    return completed
