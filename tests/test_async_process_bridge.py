"""Public Python awaitable bridge acceptance tests."""

from __future__ import annotations

import asyncio
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

    async def test_pipe_lifecycle_and_stdin_use_native_actor(self) -> None:
        if sys.platform == "win32":
            process = AsyncRunningProcess(
                "cmd.exe", ["/V:ON", "/C", "set /P input=& echo got:!input!"]
            )
        else:
            process = AsyncRunningProcess(
                "/bin/sh", ["-c", "IFS= read -r input; printf 'got:%s' \"$input\""]
            )
        await process.start()
        self.assertGreater(await process.pid(), 0)
        await process.write_stdin(b"actor-input\n")
        await process.close_stdin()
        exit_code, stdout, _stderr = await process.output()
        self.assertEqual(exit_code, 0)
        self.assertIn(b"got:actor-input", stdout)

    async def test_cancelled_wait_releases_native_actor(self) -> None:
        if sys.platform == "win32":
            process = AsyncRunningProcess("ping.exe", ["-n", "20", "127.0.0.1"])
        else:
            process = AsyncRunningProcess("/bin/sh", ["-c", "sleep 2"])
        await process.start()
        waiter = asyncio.create_task(process.wait())
        await asyncio.sleep(0.02)
        waiter.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await waiter
        await process.kill()

    async def test_pty_process_exposes_native_async_lifecycle(self) -> None:
        argv = (
            ["cmd.exe", "/C", "echo async-pty"]
            if sys.platform == "win32"
            else ["/bin/sh", "-c", "printf async-pty"]
        )
        process = AsyncPseudoTerminalProcess(argv)
        await process.start()
        self.assertGreater((await process.pid()) or 0, 0)
        await process.resize(30, 100)
        await process.read(timeout=2.0)
        await process.close()
