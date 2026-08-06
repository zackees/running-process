"""Native asyncio process facades.

The awaitables in this module are produced by the PyO3 Rust extension. They
do not delegate process work to ``asyncio.to_thread`` or an executor.
"""

from __future__ import annotations

from collections.abc import Sequence

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


__all__ = ["AsyncPseudoTerminalProcess", "AsyncRunningProcess"]
