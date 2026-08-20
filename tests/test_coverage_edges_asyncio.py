from __future__ import annotations

import asyncio
import unittest
from unittest.mock import AsyncMock, patch

import running_process.asyncio as rp_asyncio


class _FakePty:
    def __init__(self, *args: object) -> None:
        self.args = args
        self.calls: list[object] = []
        self.raise_on_poll = False

    async def start(self) -> None:
        self.calls.append("start")

    async def wait(self, timeout: float | None = None) -> int:
        self.calls.append(("wait", timeout))
        if timeout == 0.0 and self.raise_on_poll:
            raise RuntimeError("still running")
        return 4

    async def kill(self) -> None:
        self.calls.append("kill")

    async def terminate(self) -> None:
        self.calls.append("terminate")

    async def close(self) -> None:
        self.calls.append("close")

    async def read(self, timeout: float | None = None) -> bytes:
        self.calls.append(("read", timeout))
        return b"pty"

    async def write(self, data: bytes, submit: bool) -> None:
        self.calls.append(("write", data, submit))

    async def resize(self, rows: int, cols: int) -> None:
        self.calls.append(("resize", rows, cols))

    async def pid(self) -> int:
        return 101

    async def send_interrupt(self) -> None:
        self.calls.append("interrupt")

    async def kill_tree(self) -> None:
        self.calls.append("kill_tree")


class _FakePipe:
    def __init__(self, *args: object) -> None:
        self.args = args
        self.calls: list[object] = []

    async def start(self) -> None:
        self.calls.append("start")

    async def wait(self) -> int:
        self.calls.append("wait")
        return 2

    async def wait_timeout(self, timeout: float) -> int:
        self.calls.append(("wait_timeout", timeout))
        return 3

    async def kill(self) -> None:
        self.calls.append("kill")

    async def close_stdin(self) -> None:
        self.calls.append("close_stdin")

    async def output(self) -> tuple[int, bytes, bytes]:
        self.calls.append("output")
        return 2, b"out", b"err"

    async def write_stdin(self, data: bytes) -> None:
        self.calls.append(("write_stdin", data))

    async def pid(self) -> int:
        return 202

    async def poll(self) -> int | None:
        return None

    async def kill_tree(self, timeout: float) -> int:
        self.calls.append(("kill_tree", timeout))
        return 5


class AsyncInteractiveCoverageTests(unittest.IsolatedAsyncioTestCase):
    async def test_pipe_backend_routes_every_supported_operation(self) -> None:
        with patch.object(rp_asyncio, "AsyncRunningProcess", _FakePipe):
            process = rp_asyncio.AsyncInteractiveProcess("tool --flag")
            backend = process._backend
            self.assertIsInstance(backend, _FakePipe)
            self.assertEqual(backend.args, ("tool", ["--flag"]))

            await process.start()
            self.assertEqual(await process.wait(), 2)
            self.assertEqual(await process.wait(0.25), 3)
            await process.kill()
            await process.terminate()
            await process.close()
            self.assertEqual(await process.output(), (2, b"out", b"err"))
            await process.write(b"input")
            self.assertEqual(await process.pid(), 202)
            self.assertIsNone(await process.poll())
            self.assertIsNone(await process.exit_status())
            self.assertEqual(await process.kill_tree(0.5), 5)

            with self.assertRaisesRegex(RuntimeError, "expose output"):
                await process.read()
            with self.assertRaisesRegex(RuntimeError, "only applicable"):
                await process.resize(10, 20)
            with self.assertRaisesRegex(RuntimeError, "only applicable"):
                await process.send_interrupt()

    async def test_pty_backend_routes_every_supported_operation(self) -> None:
        with patch.object(rp_asyncio, "AsyncPseudoTerminalProcess", _FakePty):
            process = rp_asyncio.AsyncInteractiveProcess(
                ["tool", "arg"], use_pty=True, cwd="work", rows=30, cols=90
            )
            backend = process._backend
            self.assertIsInstance(backend, _FakePty)

            await process.start()
            self.assertEqual(await process.wait(), 4)
            self.assertEqual(await process.wait(0.5), 4)
            await process.kill()
            await process.terminate()
            await process.close()
            self.assertEqual(await process.read(0.1), b"pty")
            await process.write(b"input", submit=False)
            await process.resize(40, 120)
            self.assertEqual(await process.pid(), 101)
            self.assertEqual(await process.poll(), 4)
            self.assertEqual(await process.exit_status(), 4)
            await process.send_interrupt()
            self.assertIsNone(await process.kill_tree())

            backend.raise_on_poll = True
            self.assertIsNone(await process.poll())
            with self.assertRaisesRegex(RuntimeError, "expose read"):
                await process.output()

    async def test_empty_interactive_command_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "at least one"):
            rp_asyncio.AsyncInteractiveProcess([])


class AsyncModuleFunctionCoverageTests(unittest.IsolatedAsyncioTestCase):
    async def test_subprocess_run_success_check_and_timeout(self) -> None:
        class Process(_FakePipe):
            result = (0, b"ok", b"")

            async def output(self) -> tuple[int, bytes, bytes]:
                return self.result

        with patch.object(rp_asyncio, "AsyncRunningProcess", Process):
            result = await rp_asyncio.subprocess_run("tool arg")
            self.assertEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "ok")

            Process.result = (7, b"bad", b"failure")
            with self.assertRaises(rp_asyncio.CalledProcessError):
                await rp_asyncio.subprocess_run(["tool"], check=True)

            async def timeout(coro: object, _seconds: float) -> object:
                coro.close()  # type: ignore[attr-defined]
                raise asyncio.TimeoutError

            with patch.object(rp_asyncio.asyncio, "wait_for", timeout):
                with self.assertRaises(rp_asyncio.TimeoutExpired):
                    await rp_asyncio.subprocess_run(["tool"], timeout=0.01)

        with self.assertRaisesRegex(ValueError, "at least one"):
            await rp_asyncio.subprocess_run([])

    async def test_native_async_helpers_validate_and_project_results(self) -> None:
        kill = AsyncMock(return_value=3)
        terminate = AsyncMock(return_value=True)
        tree = AsyncMock(return_value="tree")
        find = AsyncMock(return_value=["process"])
        launch = AsyncMock(
            return_value=(9, 1.5, "tool", "work", "origin", "contained")
        )
        with (
            patch.object(rp_asyncio, "_native_kill_process_tree_async", kill),
            patch.object(rp_asyncio, "_native_terminate_process_tree_async", terminate),
            patch.object(rp_asyncio, "_native_get_process_tree_info_async", tree),
            patch.object(rp_asyncio, "_native_find_processes_by_originator_async", find),
            patch.object(rp_asyncio, "_native_launch_detached_async", launch),
        ):
            self.assertEqual(await rp_asyncio.kill_process_tree(7, 0.5), 3)
            self.assertTrue(await rp_asyncio.terminate_process_tree(8))
            self.assertEqual(await rp_asyncio.get_process_tree_info(9), "tree")
            self.assertEqual(await rp_asyncio.find_processes_by_originator("tool"), ["process"])
            detached = await rp_asyncio.launch_detached(
                "  tool  ", cwd="work", env={"A": "B"}, originator="origin"
            )
            self.assertEqual(detached.pid, 9)
            launch.assert_awaited_once_with("tool", "work", {"A": "B"}, "origin")

        with self.assertRaisesRegex(TypeError, "must be a string"):
            await rp_asyncio.launch_detached(123)  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "must not be empty"):
            await rp_asyncio.launch_detached("  ")


if __name__ == "__main__":
    unittest.main()
