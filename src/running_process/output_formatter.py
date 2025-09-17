from __future__ import annotations

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
