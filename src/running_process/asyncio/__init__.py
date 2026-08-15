"""Native asyncio process facades.

The awaitables in this module are produced by the PyO3 Rust extension. They
do not delegate process work to ``asyncio.to_thread`` or an executor.
"""

from __future__ import annotations

import asyncio
from collections.abc import Sequence
from dataclasses import dataclass
from shlex import split as split_command

from running_process._native import (
    AsyncPseudoTerminalProcess as _NativeAsyncPseudoTerminalProcess,
)
from running_process._native import (
    AsyncRunningProcess as _NativeAsyncRunningProcess,
)
from running_process._native import OriginatorProcessInfo
from running_process._native import (
    native_get_process_tree_info_async as _native_get_process_tree_info_async,
)
from running_process._native import (
    native_kill_process_tree_async as _native_kill_process_tree_async,
)
from running_process._native import (
    native_launch_detached_async as _native_launch_detached_async,
)
from running_process._native import (
    native_terminate_process_tree_async as _native_terminate_process_tree_async,
)
from running_process._native import (
    py_find_processes_by_originator_async as _native_find_processes_by_originator_async,
)
from running_process.asyncio._expect import (
    ExpectTimeoutError,
    expect_from_reader,
    wait_for_idle_from_reader,
)
from running_process.compat import CalledProcessError, CompletedProcess, TimeoutExpired
from running_process.expect import ExpectMatch, ExpectPattern
from running_process.launch import DetachedProcess
from running_process.process_watch import (
    AsyncProcessWatchCursor,
    ObservationPolicy,
    ProcessObservation,
    ProcessObservationCapabilities,
    ProcessWatch,
    ProcessWatchMatch,
)
from running_process.running_process import RunningProcess


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


#: Native PTY messages that mean "this stream has ended", per platform.
_STREAM_ENDED_MARKERS = (
    "closed",  # Windows / ConPTY
    "operation not permitted",  # macOS, once the child is reaped
    "input/output error",  # Linux, once the slave side closes
    "bad file descriptor",
)


def _is_stream_ended(error: BaseException) -> bool:
    message = str(error).lower()
    return any(marker in message for marker in _STREAM_ENDED_MARKERS)


class _CursorChunkReader:
    """Adapt a cursor to the chunk reader the matchers expect.

    Deliberately **never cancels** the underlying read. `cursor.read_next()`
    resolves a future that holds the cursor's lock on the Rust side, and
    cancelling the Python awaitable does not stop that future -- it detaches
    it, still holding the lock, so the next read blocks forever. The first
    version used `asyncio.wait_for` and hung exactly there.

    Instead one in-flight read is kept and re-awaited: a timeout leaves it
    pending and the next call picks the same one up.

    Terminal EOF raises `EOFError`, which is what stops a matcher spinning once
    the process is gone. A gap reads as empty: losing records is bad news for a
    matcher but not a reason to abandon a wait that later output may satisfy.
    Callers that cannot tolerate loss should read the cursor directly, where
    the gap is visible as an `OutputGap`.
    """

    def __init__(self, cursor: AsyncOutputCursor) -> None:
        self._cursor = cursor
        self._pending: asyncio.Future | None = None

    async def __call__(self, timeout: float | None) -> bytes | None:
        if self._pending is None:
            self._pending = asyncio.ensure_future(self._cursor.read_next())
        done, _pending = await asyncio.wait({self._pending}, timeout=timeout)
        if not done:
            return None
        finished, self._pending = self._pending, None
        read_result = finished.result()
        if read_result is None:
            raise EOFError("output cursor reached terminal EOF")
        if isinstance(read_result, OutputRecord):
            return read_result.data
        return None

    async def aclose(self) -> None:
        """Drop the in-flight read, if any.

        Cancelling here is safe in a way it is not mid-wait: nothing will use
        this cursor again, so a detached future holding its lock harms nobody.
        """
        if self._pending is not None:
            self._pending.cancel()
            self._pending = None


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
        process_watches: Sequence[ProcessWatch] | None = None,
        process_observation: ObservationPolicy = ObservationPolicy.NON_INVASIVE,
    ) -> None:
        self._watched: RunningProcess | None = None
        if process_watches:
            self._native = None
            self._watched = RunningProcess(
                [program, *args],
                auto_run=False,
                capture=True,
                text=False,
                allows_child_ctrl_c_interruption=not create_process_group,
                process_watches=process_watches,
                process_observation=process_observation,
            )
        else:
            self._native = _NativeAsyncRunningProcess(
                program, list(args), create_process_group
            )

    @staticmethod
    def process_observation_capabilities() -> ProcessObservationCapabilities:
        return RunningProcess.process_observation_capabilities()

    @property
    def process_observation(self) -> ProcessObservation | None:
        return self._watched.process_observation if self._watched is not None else None

    @property
    def watch_matches(self) -> tuple[ProcessWatchMatch, ...]:
        return self._watched.watch_matches if self._watched is not None else ()

    def process_watch_cursor(self) -> AsyncProcessWatchCursor:
        if self._watched is None:
            raise RuntimeError("this process has no process watches")
        return AsyncProcessWatchCursor(self._watched.process_watch_cursor())

    async def run(self) -> tuple[int, bytes, bytes]:
        if self._watched is not None:
            await self._watched._proc.start_async()
            return await self._watched._proc.output_async()
        assert self._native is not None
        return await self._native.run()

    async def start(self) -> None:
        if self._watched is not None:
            await self._watched._proc.start_async()
            return
        assert self._native is not None
        await self._native.start()

    async def pid(self) -> int:
        if self._watched is not None:
            pid = self._watched.pid
            if pid is None:
                raise RuntimeError("process has not started")
            return pid
        assert self._native is not None
        return await self._native.pid()

    async def wait(self) -> int:
        if self._watched is not None:
            return await self._watched._proc.wait_async()
        assert self._native is not None
        return await self._native.wait()

    async def wait_timeout(self, timeout: float) -> int:
        if self._watched is not None:
            return await self._watched._proc.wait_async(timeout)
        assert self._native is not None
        return await self._native.wait_timeout(timeout)

    async def kill(self) -> None:
        if self._watched is not None:
            await self._watched._proc.kill_async()
            return
        assert self._native is not None
        await self._native.kill()

    async def write_stdin(self, data: bytes) -> None:
        if self._watched is not None:
            await self._watched._proc.write_stdin_async(data)
            return
        assert self._native is not None
        await self._native.write_stdin(data)

    async def close_stdin(self) -> None:
        if self._watched is not None:
            await self._watched._proc.close_stdin_async()
            return
        assert self._native is not None
        await self._native.close_stdin()

    async def output(self) -> tuple[int, bytes, bytes]:
        if self._watched is not None:
            return await self._watched._proc.output_async()
        assert self._native is not None
        return await self._native.output()

    async def output_bounded(self, limit: int) -> tuple[int, bytes, bytes]:
        if self._watched is not None:
            raise RuntimeError("output_bounded is unavailable with process watches")
        assert self._native is not None
        return await self._native.output_bounded(limit)

    async def poll(self) -> int | None:
        """Exit code if the child has already exited, else ``None``.

        Never waits, matching ``RunningProcess.poll``.
        """
        if self._watched is not None:
            return await self._watched._proc.poll_async()
        assert self._native is not None
        return await self._native.poll()

    async def returncode(self) -> int | None:
        """Exit code if the child has already exited, else ``None``."""
        if self._watched is not None:
            return await self._watched._proc.poll_async()
        assert self._native is not None
        return await self._native.returncode()

    async def terminate(self) -> None:
        if self._watched is not None:
            await self._watched._proc.terminate_async()
            return
        assert self._native is not None
        await self._native.terminate()

    async def close(self) -> None:
        """Release the child handles without killing the child.

        Idempotent, and it does *not* terminate the child -- matching
        ``RunningProcess.close``. Call :meth:`kill` first if you need it gone.
        """
        if self._watched is not None:
            await self._watched._proc.close_async()
            return
        assert self._native is not None
        await self._native.close()

    async def terminate_group_soft(self) -> bool:
        """Ask the child's process group to shut down gracefully.

        Returns ``False`` when the process was not constructed with
        ``create_process_group=True``: there is no group to address, so
        nothing was signalled. An already-exited child is also ``False``.
        """
        if self._watched is not None:
            return await self._watched._proc.terminate_group_soft_async()
        assert self._native is not None
        return await self._native.terminate_group_soft()

    async def kill_tree(self, timeout: float = 5.0) -> int:
        """Kill the child and every descendant it has at this moment.

        Returns how many process instances the OS accepted a kill for.
        """
        if self._watched is not None:
            return await self._watched._proc.kill_tree_async(timeout)
        assert self._native is not None
        return await self._native.kill_tree(timeout)

    async def expect(
        self, pattern: ExpectPattern, *, timeout: float | None = None
    ) -> ExpectMatch:
        """Await output until ``pattern`` matches, or raise `ExpectTimeout`.

        Reads through a private cursor, so an expect never consumes output
        another reader was waiting for -- unlike the sync surface, where
        `expect` and `get_next_line` draw from the same buffer.

        Open it before starting a capture, for the reason `output_cursor`
        documents.
        """
        reader = _CursorChunkReader(self.output_cursor())
        try:
            return await expect_from_reader(reader, pattern, timeout=timeout)
        finally:
            await reader.aclose()

    async def wait_for_idle(
        self, idle_seconds: float, *, timeout: float | None = None
    ) -> bool:
        """Wait until no output has arrived for ``idle_seconds``.

        ``True`` if the quiet window was reached, ``False`` if ``timeout``
        elapsed first.
        """
        reader = _CursorChunkReader(self.output_cursor())
        try:
            return await wait_for_idle_from_reader(
                reader, idle_seconds, timeout=timeout
            )
        finally:
            await reader.aclose()

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

        Not pinned by a test: observing the refusal needs a capture that is
        reliably still in flight, and a short-lived child finishes first on
        a fast runner. An attempt at one passed on Windows and failed on
        macOS and Linux, which is a race, not a contract.
        """
        if self._watched is not None:
            raise RuntimeError("output_cursor is unavailable with process watches")
        assert self._native is not None
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

    def _answering_reader(self):
        """Read PTY output, answering terminal capability queries as they pass.

        A PTY child often emits a Device Status Report (`ESC [ 6 n`) during
        startup and *waits for the answer* before producing anything else. The
        sync facade's reader thread answers those, which is why its `expect`
        works; a plain `read()` loop does not, so a matcher built on it stalls
        on a child that has not actually said anything yet. This was not
        theoretical -- the async `expect` timed out against a child the sync
        one matched fine.
        """

        async def read(timeout: float | None) -> bytes | None:
            try:
                chunk = await self.read(timeout)
            except RuntimeError as error:
                # The end of a PTY stream surfaces as a RuntimeError from the
                # native layer, not as an empty read, and each platform spells
                # it differently: a closed stream on Windows, EPERM on macOS,
                # EIO on Linux. To a matcher they all mean the same thing --
                # and the difference from "nothing yet" is what stops it
                # spinning against a finished child. Matching on the message is
                # unlovely, but the errno is already flattened into text by the
                # time it reaches Python.
                if _is_stream_ended(error):
                    raise EOFError(str(error)) from error
                raise
            if chunk:
                await self.respond_to_queries(chunk)
            return chunk

        return read

    async def expect(
        self, pattern: ExpectPattern, *, timeout: float | None = None
    ) -> ExpectMatch:
        """Await PTY output until ``pattern`` matches, or raise `ExpectTimeoutError`.

        Terminal capability queries in the stream are answered as they arrive,
        matching what the sync facade's reader thread does.
        """
        return await expect_from_reader(
            self._answering_reader(), pattern, timeout=timeout
        )

    async def wait_for_output_idle(
        self, idle_seconds: float, *, timeout: float | None = None
    ) -> bool:
        """Wait until the PTY has produced no output for ``idle_seconds``.

        The counterpart of the sync `wait_for_idle` for callers that do not
        want to construct an `IdleDetectorCore`. :meth:`wait_for_idle` remains
        available for callers that do, and that share a detector with other
        machinery.
        """
        return await wait_for_idle_from_reader(
            self._answering_reader(), idle_seconds, timeout=timeout
        )

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

    async def poll(self) -> int | None:
        """Exit code if the session has already ended, else ``None``.

        Never waits, matching `InteractiveProcess.poll`. The PTY backend has no
        non-blocking exit query of its own, so it is asked for a zero-length
        wait -- which is the same question phrased the only way it accepts.
        """
        if isinstance(self._backend, AsyncPseudoTerminalProcess):
            try:
                return await self._backend.wait(0.0)
            except RuntimeError:
                return None
        return await self._backend.poll()

    async def exit_status(self) -> int | None:
        """The session's exit code once it has ended, else ``None``.

        The sync surface exposes this as a property; here it is a coroutine,
        because reaching the answer means asking the actor and that is a round
        trip, not an attribute read. Pretending otherwise would mean caching a
        value that goes stale.
        """
        return await self.poll()

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


async def terminate_process_tree(pid: int, timeout_seconds: float = 3.0) -> bool:
    """Async counterpart of :func:`running_process.terminate_process_tree`.

    Same signature, same default, same return: ``True`` also covers an
    already-exited root, which is the idempotent cleanup result callers want.
    """
    return await _native_terminate_process_tree_async(int(pid), float(timeout_seconds))


async def get_process_tree_info(pid: int) -> str:
    """Async counterpart of :func:`running_process.get_process_tree_info`.

    Enumerating the process table is a blocking snapshot, so this runs on the
    library's bounded island rather than a thread-pool bridge.
    """
    return await _native_get_process_tree_info_async(int(pid))


async def subprocess_run(
    command: str | Sequence[str],
    cwd: str | None = None,
    check: bool = False,
    timeout: float | None = None,
) -> CompletedProcess[str]:
    """Async counterpart of :func:`running_process.subprocess_run`.

    Returns the same `CompletedProcess` shape the sync helper does, so the
    result is a drop-in. Implemented over `AsyncRunningProcess`, not over the
    sync helper -- the point is that no thread is parked waiting.
    """
    argv = list(split_command(command)) if isinstance(command, str) else list(command)
    if not argv:
        raise ValueError("command must contain at least one argument")
    process = AsyncRunningProcess(argv[0], argv[1:])
    await process.start()
    if timeout is None:
        code, stdout, stderr = await process.output()
    else:
        try:
            code, stdout, stderr = await asyncio.wait_for(process.output(), timeout)
        except asyncio.TimeoutError as error:
            await process.kill()
            raise TimeoutExpired(argv, timeout) from error
    decoded_out = stdout.decode("utf-8", errors="replace")
    decoded_err = stderr.decode("utf-8", errors="replace")
    if check and code != 0:
        raise CalledProcessError(code, argv, decoded_out, decoded_err)
    return CompletedProcess(argv, code, decoded_out, decoded_err)


async def launch_detached(
    command: str,
    *,
    cwd: str | None = None,
    env: dict[str, str] | None = None,
    originator: str | None = None,
) -> DetachedProcess:
    """Async counterpart of :func:`running_process.launch_detached`.

    Keyword-only after ``command``, matching the sync helper exactly, and
    returning the same `DetachedProcess`. The spawn is the only blocking part
    and runs on the bounded island; the child is detached, so there is
    deliberately no lifecycle handle to await afterwards.
    """
    if not isinstance(command, str):
        raise TypeError("command must be a string")
    command = command.strip()
    if not command:
        raise ValueError("command must not be empty")
    entry = await _native_launch_detached_async(
        command, str(cwd) if cwd is not None else None, env, originator
    )
    return DetachedProcess(*entry)


async def find_processes_by_originator(tool: str) -> list[OriginatorProcessInfo]:
    """Async counterpart of :func:`running_process.find_processes_by_originator`.

    The scan walks the OS process table and reads each process's environment,
    which has no async form, so it runs on the bounded island.
    """
    return await _native_find_processes_by_originator_async(tool)


__all__ = [
    "AsyncInteractiveProcess",
    "AsyncOutputCursor",
    "AsyncPseudoTerminalProcess",
    "AsyncRunningProcess",
    "ExpectTimeoutError",
    "OutputGap",
    "OutputRecord",
    "find_processes_by_originator",
    "get_process_tree_info",
    "kill_process_tree",
    "launch_detached",
    "subprocess_run",
    "terminate_process_tree",
]
