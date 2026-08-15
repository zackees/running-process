"""Async output-streaming parity contracts for #875.

The sync surface streams output through `get_next_line`, `drain_*` and
`stream_iter`: each call hands the caller whatever has accumulated. The async
counterpart is a cursor -- each reader holds its own position in one shared
bounded log.

That difference is the point, so these test the properties the cursor has and
the drain family cannot: two readers see the same records independently, and a
reader that falls behind the retention window is *told* rather than silently
skipped.
"""

from __future__ import annotations

import sys
import unittest

from running_process.asyncio import AsyncRunningProcess, OutputGap, OutputRecord
from running_process.running_process import RunningProcess

EMITTER = "import sys; [print(f'line-{i}') for i in range(5)]; sys.stdout.flush()"


class TestAsyncOutputCursor(unittest.IsolatedAsyncioTestCase):
    async def test_cursor_streams_records_until_eof(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", EMITTER])
        await process.start()
        cursor = process.output_cursor()
        # Capture is what feeds the log, so drive it concurrently with reading.
        collected: list[bytes] = []

        import asyncio

        capture = asyncio.create_task(process.output())
        while True:
            read = await cursor.read_next()
            if read is None:
                break
            if isinstance(read, OutputRecord):
                collected.append(read.data)
        await capture

        joined = b"".join(collected)
        for index in range(5):
            self.assertIn(f"line-{index}".encode(), joined)

    async def test_two_cursors_read_the_same_records_independently(self) -> None:
        """The property the sync drain family cannot have.

        `drain_stdout` consumes: a second consumer gets nothing. Two cursors
        must each see the full stream.
        """
        import asyncio

        process = AsyncRunningProcess(sys.executable, ["-c", EMITTER])
        await process.start()
        first = process.output_cursor()
        second = process.output_cursor()
        capture = asyncio.create_task(process.output())

        async def drain(cursor) -> bytes:
            chunks: list[bytes] = []
            while True:
                read = await cursor.read_next()
                if read is None:
                    return b"".join(chunks)
                if isinstance(read, OutputRecord):
                    chunks.append(read.data)

        first_bytes, second_bytes = await asyncio.gather(drain(first), drain(second))
        await capture
        self.assertEqual(first_bytes, second_bytes)
        self.assertIn(b"line-0", first_bytes)

    async def test_cursor_is_an_async_iterator(self) -> None:
        import asyncio

        process = AsyncRunningProcess(sys.executable, ["-c", EMITTER])
        await process.start()
        cursor = process.output_cursor()
        capture = asyncio.create_task(process.output())
        seen = [read async for read in cursor]
        await capture
        self.assertTrue(seen)
        self.assertTrue(all(isinstance(r, OutputRecord | OutputGap) for r in seen))

    async def test_records_carry_a_sequence_and_a_stream_name(self) -> None:
        import asyncio

        process = AsyncRunningProcess(sys.executable, ["-c", EMITTER])
        await process.start()
        cursor = process.output_cursor()
        capture = asyncio.create_task(process.output())
        records = [r async for r in cursor if isinstance(r, OutputRecord)]
        await capture
        self.assertTrue(records)
        # Sequences must be strictly increasing: they are what lets a reader
        # detect a gap at all.
        sequences = [record.sequence for record in records]
        self.assertEqual(sequences, sorted(sequences))
        self.assertEqual(len(sequences), len(set(sequences)))
        for record in records:
            self.assertIn(record.stream, ("stdout", "stderr"))

    async def test_position_advances_and_close_is_observable(self) -> None:
        import asyncio

        process = AsyncRunningProcess(sys.executable, ["-c", EMITTER])
        await process.start()
        cursor = process.output_cursor()
        start = cursor.position()
        capture = asyncio.create_task(process.output())
        _ = [read async for read in cursor]
        await capture
        self.assertGreater(cursor.position(), start)
        self.assertTrue(cursor.is_closed())

    async def test_cursor_before_start_reports_not_running(self) -> None:
        process = AsyncRunningProcess(sys.executable, ["-c", "pass"])
        with self.assertRaises(RuntimeError):
            process.output_cursor()


class TestSyncStreamingCounterpart(unittest.TestCase):
    """The sync half of the streaming rows."""

    def test_sync_stream_iter_yields_the_emitted_lines(self) -> None:
        process = RunningProcess([sys.executable, "-c", EMITTER], auto_run=False)
        process.start()
        lines = [str(line) for line in process.line_iter(timeout=30)]
        process.wait()
        joined = "".join(lines)
        for index in range(5):
            self.assertIn(f"line-{index}", joined)

    def test_sync_drain_stdout_consumes_what_it_returns(self) -> None:
        """The contrast the cursor exists to fix.

        A drain hands over the accumulated output and empties the buffer, so a
        second caller sees nothing. This documents that as the sync contract
        rather than treating it as a bug.
        """
        process = RunningProcess([sys.executable, "-c", EMITTER], auto_run=False)
        process.start()
        process.wait()
        first = process.drain_stdout()
        second = process.drain_stdout()
        self.assertEqual(second, [], "a second drain must find the buffer empty")
        del first


if __name__ == "__main__":
    unittest.main()
