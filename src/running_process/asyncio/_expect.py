"""Async expect matching for the native asyncio process facades.

The synchronous `expect` is Python code over native reads: it accumulates
decoded output and runs :func:`search_expect_pattern` against the buffer. This
is the same code over native *awaits*.

That symmetry is the point. Nothing here polls a synchronous API or hands work
to a thread pool -- the only thing it waits on is the awaitable the Rust
extension returns, so cancelling an expect cancels the read underneath it.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable

from running_process.expect import ExpectMatch, ExpectPattern, search_expect_pattern

#: Longest a single read may park before the matcher re-checks its deadline.
#: This is a fairness bound, not a responsiveness one: the PTY reader occupies
#: a blocking-island permit for exactly as long as it is given.
READ_SLICE_SECONDS = 0.25

#: A reader takes the time it may wait and returns bytes, or ``None`` if that
#: window produced nothing. It raises :class:`EOFError` when the stream has
#: ended, which is what distinguishes "nothing yet" from "nothing ever" -- a
#: matcher that could not tell those apart would spin forever after exit.
ChunkReader = Callable[[float | None], Awaitable[bytes | None]]


class ExpectTimeoutError(TimeoutError):
    """Raised when no pattern matched before the deadline.

    Subclasses :class:`TimeoutError`, which is what the synchronous `expect`
    raises, so a caller's existing handler keeps working. The buffer is
    attached because "what *did* arrive" is the first question anyone asks
    when an expect times out.
    """

    def __init__(self, pattern: ExpectPattern, buffer: str) -> None:
        super().__init__(f"Pattern not found before timeout: {pattern!r}")
        self.pattern = pattern
        self.buffer = buffer


async def expect_from_reader(
    read_chunk: ChunkReader,
    pattern: ExpectPattern,
    *,
    timeout: float | None = None,
    encoding: str = "utf-8",
) -> ExpectMatch:
    """Accumulate output from ``read_chunk`` until ``pattern`` matches.

    Raises :class:`ExpectTimeoutError` on deadline and :class:`EOFError` when
    the stream ends first -- the same two outcomes, and the same exception
    types, as the synchronous `expect`.

    Decoding uses ``errors="replace"``: a chunk boundary can split a multi-byte
    character, and a replacement character is a worse match than the real text
    but a far better outcome than a decode error aborting the wait.
    """
    loop = asyncio.get_running_loop()
    deadline = None if timeout is None else loop.time() + timeout
    buffer = ""

    while True:
        match = search_expect_pattern(buffer, pattern)
        if match is not None:
            return match

        remaining: float | None = None
        if deadline is not None:
            remaining = deadline - loop.time()
            if remaining <= 0:
                raise ExpectTimeoutError(pattern, buffer)

        # A bounded slice, never the whole remaining budget. The PTY reader
        # holds one of the blocking island's two permits for as long as it is
        # given, so a single 20s read starves every other PTY operation in the
        # process -- including the `close()` in the caller's own `finally`.
        # Slicing costs a wakeup per tick and buys back that fairness. It is
        # safe for the cursor reader too, because that one re-awaits a pending
        # read rather than cancelling it.
        window = READ_SLICE_SECONDS
        if remaining is not None:
            window = min(window, remaining)
        try:
            chunk = await read_chunk(window)
        except EOFError as error:
            raise EOFError(
                f"Stream ended before pattern matched: {pattern!r}"
            ) from error

        if chunk:
            buffer += chunk.decode(encoding, errors="replace")


async def wait_for_idle_from_reader(
    read_chunk: ChunkReader,
    idle_seconds: float,
    *,
    timeout: float | None = None,
) -> bool:
    """Wait until no output has arrived for ``idle_seconds``.

    Returns ``True`` when the quiet window was reached, ``False`` when
    ``timeout`` elapsed first. A stream that ends counts as idle: nothing more
    can arrive, so the quiet window is satisfied for good.

    Idle is measured from the last chunk *observed here*, so a caller reading
    the same process elsewhere resets nothing. Give each waiter its own cursor
    if that matters.
    """
    loop = asyncio.get_running_loop()
    deadline = None if timeout is None else loop.time() + timeout

    while True:
        if deadline is not None and loop.time() >= deadline:
            return False

        window = idle_seconds
        if deadline is not None:
            window = min(window, max(deadline - loop.time(), 0.0))

        # A read that waits `idle_seconds` and returns nothing *is* the idle
        # observation -- no separate clock needed.
        try:
            chunk = await read_chunk(window)
        except EOFError:
            return True

        if not chunk and window >= idle_seconds:
            return True
