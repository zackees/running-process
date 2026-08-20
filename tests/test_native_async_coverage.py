"""Direct coverage for the bounded native async process surface."""

from __future__ import annotations

import sys
import unittest

from running_process._native import NativeProcess


def _native(script: str, *, process_group: bool = False) -> NativeProcess:
    return NativeProcess(
        [sys.executable, "-c", script],
        stdin_mode_name="piped",
        stderr_mode_name="pipe",
        create_process_group=process_group,
    )


class TestNativeAsyncCoverage(unittest.IsolatedAsyncioTestCase):
    async def test_native_async_process_methods_complete_on_bounded_island(self) -> None:
        stream = _native(
            "import sys; data=sys.stdin.buffer.read(); "
            "sys.stdout.buffer.write(data.upper()); sys.stderr.write('err')"
        )
        try:
            await stream.start_async()
            self.assertIsNone(await stream.poll_async())
            await stream.write_stdin_async(b"coverage")
            await stream.close_stdin_async()
            code, stdout, stderr = await stream.output_async()
            self.assertEqual(code, 0)
            self.assertEqual(stdout, b"COVERAGE")
            self.assertEqual(stderr, b"err")
            self.assertEqual(await stream.poll_async(), 0)
            self.assertFalse(await stream.terminate_group_soft_async())
        finally:
            await stream.close_async()

        with self.assertRaisesRegex(ValueError, "finite non-negative"):
            stream.wait_async(-1.0)
        with self.assertRaisesRegex(ValueError, "finite non-negative"):
            stream.kill_tree_async(-1.0)

        not_started = _native("pass")
        try:
            self.assertEqual(await not_started.kill_tree_async(0.1), 0)
        finally:
            await not_started.close_async()

        terminate = _native("import time; time.sleep(30)")
        try:
            await terminate.start_async()
            await terminate.terminate_async()
            self.assertNotEqual(await terminate.wait_async(10.0), 0)
        finally:
            await terminate.close_async()

        kill = _native("import time; time.sleep(30)")
        try:
            await kill.start_async()
            await kill.kill_async()
            self.assertNotEqual(await kill.wait_async(10.0), 0)
        finally:
            await kill.close_async()

        group = _native("import time; time.sleep(30)", process_group=True)
        try:
            await group.start_async()
            self.assertTrue(await group.terminate_group_soft_async())
            self.assertNotEqual(await group.wait_async(10.0), 0)
        finally:
            await group.close_async()

        tree = _native("import time; time.sleep(30)")
        try:
            await tree.start_async()
            self.assertGreaterEqual(await tree.kill_tree_async(10.0), 1)
            self.assertNotEqual(await tree.wait_async(10.0), 0)
        finally:
            await tree.close_async()
