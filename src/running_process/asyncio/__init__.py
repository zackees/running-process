"""Native asyncio process facades.

The awaitables in this module are produced by the PyO3 Rust extension. They
do not delegate process work to ``asyncio.to_thread`` or an executor.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
from shlex import split as split_command

from running_process._native import (
    AsyncPseudoTerminalProcess as _NativeAsyncPseudoTerminalProcess,
)
from running_process._native import (
    AsyncRunningProcess as _NativeAsyncRunningProcess,
)
from running_process._native import (
    native_kill_process_tree_async as _native_kill_process_tree_async,
)


@dataclass(frozen=True)
class OutputRecord:
    """One sequenced chunk of output observed on a stream."""

    sequence: int
    stream: str
    data: bytes


@dataclass(frozen=True)
class OutputGap:
    """Records this reader missed because the retention window moved past it.

    Reported rather than skipped. The log is byte-bounded, so a slow consumer
    genuinely can lose records; a cursor that hid that would let the consumer
    believe it had seen everything.
    """

    first_missing: int
    last_missing: int


class AsyncOutputCursor:
    """An independent reader position over one process's retained output."""

    def __init__(self, native: object) -> None:
        self._native = native

    async def read_next(self) -> OutputRecord | OutputGap | None:
        """Await the next record or gap. ``None`` means terminal EOF."""
        read = await self._native.read_next()  # type: ignore[attr-defined]
        if read is None:
            return None
        kind, first, second, stream, data = read
        if kind == "gap":
            return OutputGap(first_missing=first, last_missing=second)
        return OutputRecord(sequence=first, stream=stream, data=data)

    def position(self) -> int:
        """The next sequence this cursor will request.

        Raises ``RuntimeError`` if a :meth:`read_next` on this same cursor is
        still in flight. Reading blocks until a record arrives, so answering
        during one would mean either blocking the calling thread or reporting a
        position that is about to change; refusing is the honest option. Query
        between reads, or give each task its own cursor.

        Deliberately not pinned by a test: reproducing it needs a read that
        stays blocked, and a test that parks on an indefinite read is a hang
        waiting to happen in CI.
        """
        return self._native.position()  # type: ignore[attr-defined]

    def is_closed(self) -> bool:
        """Whether the producer has closed the log.

        Same in-flight-read caveat as :meth:`position`.
        """
        return self._native.is_closed()  # type: ignore[attr-defined]

    def __aiter__(self) -> AsyncOutputCursor:
        return self

    async def __anext__(self) -> OutputRecord | OutputGap:
        read = await self.read_next()
        if read is None:
            raise StopAsyncIteration
        return read


class AsyncRunningProcess:
    """Async pipe process backed by native Rust futures.

    ``run`` and ``output`` return ``(exit_code, stdout_bytes, stderr_bytes)``.
    Every lifecycle operation is delegated to the same actor-owned process.
    """

    def __init__(
        self,
        program: str,
        args: Sequence[str] = (),
        *,
        create_process_group: bool = False,
    ) -> None:
        self._native = _NativeAsyncRunningProcess(
            program, list(args), create_process_group
        )

    async def run(self) -> tuple[int, bytes, bytes]:
        return await self._native.run()

    async def start(self) -> None:
        await self._native.start()

    async def pid(self) -> int:
        return await self._native.pid()

    async def wait(self) -> int:
        return await self._native.wait()

    async def wait_timeout(self, timeout: float) -> int:
        return await self._native.wait_timeout(timeout)

    async def kill(self) -> None:
        await self._native.kill()

    async def write_stdin(self, data: bytes) -> None:
        await self._native.write_stdin(data)

    async def close_stdin(self) -> None:
        await self._native.close_stdin()

    async def output(self) -> tuple[int, bytes, bytes]:
        return await self._native.output()

    async def output_bounded(self, limit: int) -> tuple[int, bytes, bytes]:
        return await self._native.output_bounded(limit)

    async def poll(self) -> int | None:
        """Exit code if the child has already exited, else ``None``.

        Never waits, matching ``RunningProcess.poll``.
        """
        return await self._native.poll()

    async def returncode(self) -> int | None:
        """Exit code if the child has already exited, else ``None``."""
        return await self._native.returncode()

    async def terminate(self) -> None:
        await self._native.terminate()

    async def close(self) -> None:
        """Release the child handles without killing the child.

        Idempotent, and it does *not* terminate the child -- matching
        ``RunningProcess.close``. Call :meth:`kill` first if you need it gone.
        """
        await self._native.close()

    async def terminate_group_soft(self) -> bool:
        """Ask the child's process group to shut down gracefully.

        Returns ``False`` when the process was not constructed with
        ``create_process_group=True``: there is no group to address, so
        nothing was signalled. An already-exited child is also ``False``.
        """
        return await self._native.terminate_group_soft()

    async def kill_tree(self, timeout: float = 5.0) -> int:
        """Kill the child and every descendant it has at this moment.

        Returns how many process instances the OS accepted a kill for.
        """
        return await self._native.kill_tree(timeout)

    def output_cursor(self) -> AsyncOutputCursor:
        """Open an independent cursor over the output the actor has retained.

        The async counterpart of the sync drain/read family. Those hand a
        caller whatever has accumulated; a cursor gives each reader its own
        position in one shared bounded log, so two consumers cannot steal
        records from each other and a reader that falls behind is *told* so.

        Synchronous by design -- opening a cursor only clones a handle. The
        awaiting happens on :meth:`AsyncOutputCursor.read_next`.

        Open the cursor **before** starting a capture. :meth:`output` holds the
        process for its whole duration, so a cursor opened while one is in
        flight raises ``RuntimeError`` rather than blocking the event loop
        waiting for it. Opening first is also what you want semantically: a
        cursor created after capture began has already missed records.
        """
        return AsyncOutputCursor(self._native.output_cursor())


class AsyncPseudoTerminalProcess:
    """Async PTY facade backed by the bounded native PTY island."""

    def __init__(
        self,
        argv: Sequence[str],
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        rows: int = 24,
        cols: int = 80,
        nice: int | None = None,
    ) -> None:
        self._native = _NativeAsyncPseudoTerminalProcess(
            list(argv), cwd, env, rows, cols, nice
        )

    async def start(self) -> None:
        await self._native.start()

    async def read(self, timeout: float | None = None) -> bytes | None:
        return await self._native.read(timeout)

    async def close(self) -> None:
        await self._native.close()

    async def write(self, data: bytes, submit: bool = True) -> None:
        await self._native.write(data, submit)

    async def resize(self, rows: int, cols: int) -> None:
        await self._native.resize(rows, cols)

    async def wait(self, timeout: float | None = None) -> int:
        return await self._native.wait(timeout)

    async def terminate(self) -> None:
        await self._native.terminate()

    async def kill(self) -> None:
        await self._native.kill()

    async def pid(self) -> int | None:
        return await self._native.pid()

    async def send_interrupt(self) -> None:
        """Deliver Ctrl+C / SIGINT to the PTY child."""
        await self._native.send_interrupt()

    async def terminate_tree(self) -> None:
        await self._native.terminate_tree()

    async def kill_tree(self) -> None:
        await self._native.kill_tree()

    async def respond_to_queries(self, data: bytes) -> None:
        """Answer terminal capability queries found in a PTY output chunk."""
        await self._native.respond_to_queries(data)

    async def wait_and_drain(
        self, timeout: float | None = None, drain_timeout: float = 1.0
    ) -> int:
        """Wait for exit, then drain output still in flight.

        Exit and EOF are separate events on a PTY: a child can exit while the
        master still holds buffered output, so a plain :meth:`wait` can leave
        bytes unread.
        """
        return await self._native.wait_and_drain(timeout, drain_timeout)

    async def wait_for_reader_closed(self, timeout: float | None = None) -> bool:
        """Wait until the PTY reader closes. ``False`` means it timed out."""
        return await self._native.wait_for_reader_closed(timeout)

    async def start_terminal_input_relay(self) -> None:
        await self._native.start_terminal_input_relay()

    async def stop_terminal_input_relay(self) -> None:
        await self._native.stop_terminal_input_relay()

    # Below here the calls are synchronous, not awaitable. They touch only
    # atomics, so an await would add scheduling cost for no gain -- and it
    # would make them unusable from the teardown paths (signal handlers,
    # __del__) that need them most, because those cannot await.

    def request_terminal_input_relay_stop(self) -> None:
        self._native.request_terminal_input_relay_stop()

    def terminal_input_relay_active(self) -> bool:
        return self._native.terminal_input_relay_active()

    def set_echo(self, enabled: bool) -> None:
        self._native.set_echo(enabled)

    def echo_enabled(self) -> bool:
        return self._native.echo_enabled()

    def close_nonblocking(self) -> None:
        self._native.close_nonblocking()

    def mark_reader_closed(self) -> None:
        self._native.mark_reader_closed()

    def store_returncode(self, code: int) -> None:
        self._native.store_returncode(code)

    def record_input_metrics(self, data: bytes, submit: bool) -> None:
        self._native.record_input_metrics(data, submit)

    def pty_input_bytes_total(self) -> int:
        return self._native.pty_input_bytes_total()

    def pty_newline_events_total(self) -> int:
        return self._native.pty_newline_events_total()

    def pty_submit_events_total(self) -> int:
        return self._native.pty_submit_events_total()

    def pty_output_bytes_total(self) -> int:
        return self._native.pty_output_bytes_total()

    def pty_control_churn_bytes_total(self) -> int:
        return self._native.pty_control_churn_bytes_total()


class AsyncInteractiveProcess:
    """Async dispatch facade for pipe or PTY interactive sessions.

    ``use_pty=False`` selects the Tokio pipe actor; ``use_pty=True`` selects
    the bounded native PTY facade.  A string command is parsed with
    :func:`shlex.split` and is never routed through the synchronous facade.
    """

    def __init__(
        self,
        command: str | Sequence[str],
        *,
        use_pty: bool = False,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        rows: int = 24,
        cols: int = 80,
        nice: int | None = None,
    ) -> None:
        argv = list(split_command(command)) if isinstance(command, str) else list(command)
        if not argv:
            raise ValueError("command must contain at least one argument")
        if use_pty:
            self._backend: AsyncRunningProcess | AsyncPseudoTerminalProcess = (
                AsyncPseudoTerminalProcess(argv, cwd, env, rows, cols, nice)
            )
        else:
            self._backend = AsyncRunningProcess(argv[0], argv[1:])

    async def start(self) -> None:
        await self._backend.start()

    async def wait(self, timeout: float | None = None) -> int:
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            return await self._backend.wait(timeout)
        if timeout is None:
            return await self._backend.wait()
        return await self._backend.wait_timeout(timeout)

    async def kill(self) -> None:
        await self._backend.kill()

    async def terminate(self) -> None:
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            await self._backend.terminate()
        else:
            await self._backend.kill()

    async def close(self) -> None:
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            await self._backend.close()
        else:
            await self._backend.close_stdin()

    async def output(self) -> tuple[int, bytes, bytes]:
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            raise RuntimeError("PTY interactive sessions expose read(), not output()")
        return await self._backend.output()

    async def read(self, timeout: float | None = None) -> bytes | None:
        if not isinstance(self._backend, AsyncPseudoTerminalProcess):
            raise RuntimeError("pipe interactive sessions expose output(), not read()")
        return await self._backend.read(timeout)

    async def write(self, data: bytes, submit: bool = True) -> None:
        if not isinstance(self._backend, AsyncPseudoTerminalProcess):
            await self._backend.write_stdin(data)
        else:
            await self._backend.write(data, submit)

    async def resize(self, rows: int, cols: int) -> None:
        if not isinstance(self._backend, AsyncPseudoTerminalProcess):
            raise RuntimeError("resize() is only applicable to PTY sessions")
        await self._backend.resize(rows, cols)

    async def pid(self) -> int | None:
        return await self._backend.pid()

    async def send_interrupt(self) -> None:
        if not isinstance(self._backend, AsyncPseudoTerminalProcess):
            raise RuntimeError("send_interrupt() is only applicable to PTY sessions")
        await self._backend.send_interrupt()

    async def kill_tree(self, timeout: float = 5.0) -> int | None:
        """Kill the session's process tree.

        The pipe backend reports how many instances were killed; the PTY
        backend's tree kill has no count to report and returns ``None``.
        """
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            await self._backend.kill_tree()
            return None
        return await self._backend.kill_tree(timeout)


async def kill_process_tree(pid: int, timeout_seconds: float = 3.0) -> int:
    """Async counterpart of :func:`running_process.kill_process_tree`.

    The parameter name and default match the sync helper exactly; a caller
    porting a call should only have to add ``await``.

    Returns the number of process instances the OS accepted a kill for, which
    the synchronous helper discards.

    This runs on the library's bounded native blocking island, not
    ``asyncio.to_thread``: enumerating the OS process table has no async form,
    and a thread-pool bridge would put an unbounded pool between the caller and
    the OS while claiming to be async.
    """
    return await _native_kill_process_tree_async(int(pid), float(timeout_seconds))


__all__ = [
    "AsyncInteractiveProcess",
    "AsyncOutputCursor",
    "AsyncPseudoTerminalProcess",
    "AsyncRunningProcess",
    "OutputGap",
    "OutputRecord",
    "kill_process_tree",
]
