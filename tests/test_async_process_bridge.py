"""Public Python awaitable bridge acceptance tests."""

from __future__ import annotations

import sys
import unittest

from running_process.asyncio import AsyncPseudoTerminalProcess, AsyncRunningProcess


class TestAsyncProcessBridge(unittest.IsolatedAsyncioTestCase):
    async def test_pipe_process_uses_native_awaitable(self) -> None:
        if sys.platform == "win32":
            process = AsyncRunningProcess("cmd.exe", ["/C", "echo async-python"])
        else:
            process = AsyncRunningProcess("/bin/sh", ["-c", "printf async-python"])
        exit_code, stdout, _stderr = await process.run()
        self.assertEqual(exit_code, 0)
        self.assertIn(b"async-python", stdout)

    async def test_pty_process_exposes_native_async_lifecycle(self) -> None:
        argv = (
            ["cmd.exe", "/C", "echo async-pty"]
            if sys.platform == "win32"
            else ["/bin/sh", "-c", "printf async-pty"]
        )
        process = AsyncPseudoTerminalProcess(argv)
        await process.start()
        await process.read(timeout=2.0)
        await process.close()
