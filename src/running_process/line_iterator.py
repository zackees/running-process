"""Line iterator module.

This module contains the _RunningProcessLineIterator class for iterating over
process output lines in a context-managed way.
"""

from collections.abc import Iterator
from contextlib import AbstractContextManager
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from running_process.running_process import RunningProcess

from running_process.process_output_reader import EndOfStream


class _RunningProcessLineIterator(AbstractContextManager[Iterator[str]], Iterator[str]):
    """Context-managed iterator over a RunningProcess's output lines.

    Yields only strings (never None). Stops on EndOfStream or when a per-line
    timeout elapses.
    """

    def __init__(self, rp: "RunningProcess", timeout: float | None) -> None:
        self._rp = rp
        self._timeout = timeout

    # Context manager protocol
    def __enter__(self) -> "_RunningProcessLineIterator":
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: Any | None,
    ) -> bool:
        # Do not suppress exceptions
        return False

    # Iterator protocol
    def __iter__(self) -> Iterator[str]:
        return self

    def __next__(self) -> str:
        next_item: str | EndOfStream = self._rp.get_next_line(timeout=self._timeout)

        if isinstance(next_item, EndOfStream):
            raise StopIteration

        # Must be a string by contract
        return next_item
