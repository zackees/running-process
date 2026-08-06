"""Native asyncio process facades.

The awaitables in this module are produced by the PyO3 Rust extension. They
do not delegate process work to ``asyncio.to_thread`` or an executor.
"""

from __future__ import annotations

from collections.abc import Sequence
from shlex import split as split_command

from running_process._native import (
    AsyncPseudoTerminalProcess as _NativeAsyncPseudoTerminalProcess,
)
from running_process._native import (
    AsyncRunningProcess as _NativeAsyncRunningProcess,
)


class AsyncRunningProcess:
    """Async pipe process backed by native Rust futures.

    ``run`` and ``output`` return ``(exit_code, stdout_bytes, stderr_bytes)``.
    Every lifecycle operation is delegated to the same actor-owned process.
    """

    def __init__(self, program: str, args: Sequence[str] = ()) -> None:
        self._native = _NativeAsyncRunningProcess(program, list(args))

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


__all__ = [
    "AsyncInteractiveProcess",
    "AsyncPseudoTerminalProcess",
    "AsyncRunningProcess",
]
