from __future__ import annotations

import time
from typing import Protocol


class OutputFormatter(Protocol):
    """Protocol for output formatters used with RunningProcess."""

    def begin(self) -> None: ...

    def transform(self, line: str) -> str: ...

    def end(self) -> None: ...


class NullOutputFormatter:
    """No-op formatter that returns input unchanged and has no lifecycle effects."""

    def begin(self) -> None:
        return None

    def transform(self, line: str) -> str:
        return line

    def end(self) -> None:
        return None


class TimeDeltaFormatter:
    """Formatter that prefixes each line with time elapsed since process start.

    Example output format: "[1.23] Hello world"
    """

    def __init__(self, start_time: float | None = None) -> None:
        """Initialize the formatter.

        Args:
            start_time: Process start time. If None, will be set when begin() is called.
                       Pass the RunningProcess.start_time for accurate timing.
        """
        self._start_time = start_time

    def begin(self) -> None:
        """Initialize the formatter start time if not already set."""
        if self._start_time is None:
            self._start_time = time.time()

    def transform(self, line: str) -> str:
        """Add time delta prefix to the line.

        Args:
            line: Input line to transform

        Returns:
            Line prefixed with elapsed time in format "[0.00] original_line"
        """
        if self._start_time is None:
            # Fallback if begin() wasn't called
            self._start_time = time.time()
            elapsed = 0.0
        else:
            elapsed = time.time() - self._start_time

        return f"[{elapsed:.2f}] {line}"

    def end(self) -> None:
        """Finalize the formatter."""
        return
