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
    """One-shot async pipe process backed by a native Rust future.

    ``run`` returns ``(exit_code, stdout_bytes, stderr_bytes)``. The initial
    bridge intentionally exposes raw bytes so no Python-side polling or
    reader thread is needed; richer streaming methods will be added against
    the actor cursor contract.
    """

    def __init__(self, program: str, args: Sequence[str] = ()) -> None:
        self._native = _NativeAsyncRunningProcess(program, list(args))

    async def run(self) -> tuple[int, bytes, bytes]:
        return await self._native.run()


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


__all__ = ["AsyncPseudoTerminalProcess", "AsyncRunningProcess"]
