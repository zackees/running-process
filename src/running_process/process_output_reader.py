"""Process output reader module.

This module contains the ProcessOutputReader class for handling subprocess output
in a dedicated thread to prevent blocking issues.
"""

import _thread
import logging
import os
import re
import signal
import sys
import threading
import time
import traceback
import warnings
from collections.abc import Callable
from subprocess import Popen
from typing import TYPE_CHECKING, Any, Union

if TYPE_CHECKING:
    from running_process.pty import PtyProcessProtocol

from running_process.output_formatter import NullOutputFormatter, OutputFormatter

logger = logging.getLogger(__name__)


class EndOfStream:
    """Sentinel used to indicate end-of-stream from the reader."""


class ProcessOutputReader:
    """Dedicated reader that drains a process's stdout and enqueues lines.

    This keeps the stdout pipe drained to prevent blocking and forwards
    transformed, non-empty lines to the provided output queue. It also invokes
    lifecycle callbacks for timing/unregister behaviors.
    """

    def __init__(
        self,
        proc: Union[Popen[Any], "PtyProcessProtocol"],
        shutdown: threading.Event,
        output_formatter: OutputFormatter | None,
        on_output: Callable[[str | EndOfStream], None],
        on_end: Callable[[], None],
        use_pty: bool = False,
        pty_proc: Any = None,
        pty_master_fd: int | None = None,
    ) -> None:
        output_formatter = output_formatter or NullOutputFormatter()
        self._proc = proc
        self._shutdown = shutdown
        self._output_formatter = output_formatter
        self._on_output = on_output
        self._on_end = on_end
        self.last_stdout_ts: float | None = None
        self._eos_emitted: bool = False
        self._use_pty = use_pty
        self._pty_proc = pty_proc
        self._pty_master_fd = pty_master_fd
        # Compile ANSI escape sequence regex for PTY output filtering
        self._ansi_escape = re.compile(r"\x1b\[[^a-zA-Z]*[a-zA-Z]") if use_pty else None

    def _emit_eos_once(self) -> None:
        """Ensure EndOfStream is only forwarded a single time."""
        if not self._eos_emitted:
            self._eos_emitted = True
            self._on_output(EndOfStream())

    def _initialize_formatter(self) -> None:
        """Initialize the output formatter."""
        try:
            self._output_formatter.begin()
        except (AttributeError, TypeError, ValueError, RuntimeError) as e:
            formatter_error_msg = f"Output formatter begin() failed: {e}"
            warnings.warn(formatter_error_msg, stacklevel=2)

    def _process_stdout_lines(self) -> None:
        """Process stdout lines and forward them to output."""
        if self._use_pty:
            self._process_pty_output()
        else:
            self._process_pipe_output()

    def _process_pipe_output(self) -> None:
        """Process standard pipe output."""
        assert self._proc.stdout is not None

        while True:
            if self._shutdown.is_set():
                break

            line = self._proc.stdout.readline()
            if not line:  # EOF reached
                break

            self.last_stdout_ts = time.time()

            line_stripped = line.rstrip()
            if not line_stripped:
                continue

            transformed_line = self._output_formatter.transform(line_stripped)
            self._on_output(transformed_line)

    def _read_pty_chunk(self) -> str | None:
        """Read a chunk of data from PTY."""
        if sys.platform == "win32" and self._pty_proc:
            # Windows: read from winpty
            chunk = self._pty_proc.read()
            return chunk if chunk else None
        if self._pty_master_fd is not None:
            # Unix: read from PTY file descriptor
            import select  # noqa: PLC0415

            # Use select to check if data is available with timeout
            ready, _, _ = select.select([self._pty_master_fd], [], [], 0.1)
            if not ready:
                return ""  # No data available, continue
            chunk_bytes = os.read(self._pty_master_fd, 4096)
            if not chunk_bytes:
                return None  # EOF
            return chunk_bytes.decode("utf-8", errors="replace")
        return None  # No PTY available

    def _process_pty_chunk(self, chunk: str, buffer: str) -> str:
        """Process a chunk of PTY data and return updated buffer."""
        self.last_stdout_ts = time.time()

        # Filter ANSI escape sequences if regex is available
        if self._ansi_escape:
            chunk = self._ansi_escape.sub("", chunk)

        # Normalize line endings and add to buffer
        chunk = chunk.replace("\r\n", "\n").replace("\r", "\n")
        buffer += chunk

        # Process complete lines from buffer
        while "\n" in buffer:
            line, buffer = buffer.split("\n", 1)
            line = line.rstrip()
            if line:
                transformed_line = self._output_formatter.transform(line)
                self._on_output(transformed_line)

        return buffer

    def _process_pty_output(self) -> None:  # noqa: C901
        """Process PTY output with ANSI filtering."""
        buffer = ""

        while not self._shutdown.is_set():
            try:
                chunk = self._read_pty_chunk()
                if chunk is None:
                    break  # EOF or no PTY
                if chunk == "":
                    continue  # No data available, continue
                if chunk:
                    buffer = self._process_pty_chunk(chunk, buffer)

            except KeyboardInterrupt:
                # CRITICAL: Handle KeyboardInterrupt in PTY mode
                logger.warning("KeyboardInterrupt in PTY output reader - cleaning up PTY process")
                # Clean up PTY process immediately
                if sys.platform == "win32" and self._pty_proc:
                    try:
                        self._pty_proc.kill(signal.SIGTERM)
                    except (OSError, ValueError, RuntimeError) as e:
                        logger.warning("Failed to kill winpty process on KeyboardInterrupt: %s", e)
                # Re-raise to be handled by the main handler
                raise
            except (OSError, ValueError) as e:
                # PTY closed or error reading
                logger.debug("PTY read error (normal on close): %s", e)
                break
            except Exception as e:  # noqa: BLE001
                # Unexpected error, log it for debugging
                logger.warning("Unexpected error in PTY reader: %s", e)
                break

        # Process any remaining data in buffer
        if buffer and buffer.strip():
            self.last_stdout_ts = time.time()
            for raw_line in buffer.split("\n"):
                line = raw_line.rstrip()
                if line:
                    transformed_line = self._output_formatter.transform(line)
                    self._on_output(transformed_line)

    def _handle_keyboard_interrupt(self) -> None:
        """Handle KeyboardInterrupt in reader thread."""
        # Per project rules, handle interrupts in threads explicitly
        thread_id = threading.current_thread().ident
        thread_name = threading.current_thread().name
        logger.warning("Thread %s (%s) caught KeyboardInterrupt", thread_id, thread_name)
        logger.warning("Stack trace for thread %s:", thread_id)
        traceback.print_exc()
        # Try to ensure child process is terminated promptly
        try:
            self._proc.kill()
        except (ProcessLookupError, PermissionError, OSError) as kill_error:
            logger.warning("Failed to kill process: %s", kill_error)
        # Propagate to main thread and re-raise
        _thread.interrupt_main()
        # EOF
        self._emit_eos_once()

    def _handle_io_error(self, e: ValueError | OSError) -> None:
        """Handle IO errors during stdout reading."""
        # Normal shutdown scenarios include closed file descriptors.
        error_str = str(e)
        if any(msg in error_str for msg in ["closed file", "Bad file descriptor"]):
            closed_file_msg = f"Output reader encountered closed file: {e}"
            warnings.warn(closed_file_msg, stacklevel=2)
        else:
            logger.warning("Output reader encountered error: %s", e)

    def _cleanup_stdout(self) -> None:
        """Close stdout stream safely."""
        if self._use_pty:
            # PTY cleanup
            if sys.platform == "win32" and self._pty_proc:
                try:
                    self._pty_proc.close()
                except (ValueError, OSError) as err:
                    reader_error_msg = f"PTY reader encountered error: {err}"
                    warnings.warn(reader_error_msg, stacklevel=2)
            elif self._pty_master_fd is not None:
                try:
                    os.close(self._pty_master_fd)
                except (ValueError, OSError) as err:
                    reader_error_msg = f"PTY reader encountered error: {err}"
                    warnings.warn(reader_error_msg, stacklevel=2)
        # Standard pipe cleanup
        elif self._proc.stdout and not self._proc.stdout.closed:
            try:
                self._proc.stdout.close()
            except (ValueError, OSError) as err:
                reader_error_msg = f"Output reader encountered error: {err}"
                warnings.warn(reader_error_msg, stacklevel=2)

    def _finalize_formatter(self) -> None:
        """Finalize the output formatter."""
        try:
            self._output_formatter.end()
        except (AttributeError, TypeError, ValueError, RuntimeError) as e:
            formatter_end_error_msg = f"Output formatter end() failed: {e}"
            warnings.warn(formatter_end_error_msg, stacklevel=2)

    def _run_with_error_handling(self) -> None:
        """Run stdout processing with error handling."""
        try:
            self._process_stdout_lines()
        except KeyboardInterrupt:
            self._handle_keyboard_interrupt()
            raise
        except (ValueError, OSError) as e:
            self._handle_io_error(e)
        finally:
            # Signal end-of-stream to consumers exactly once
            self._emit_eos_once()

    def _perform_final_cleanup(self) -> None:
        """Perform final cleanup operations."""
        # Cleanup stream and invoke completion callback
        self._cleanup_stdout()

        # Notify parent for timing/unregistration
        try:
            self._on_end()
        finally:
            # End formatter lifecycle within the reader context
            self._finalize_formatter()

    def run(self) -> None:
        """Continuously read stdout lines and forward them until EOF or shutdown."""
        try:
            # Begin formatter lifecycle within the reader context
            self._initialize_formatter()
            self._run_with_error_handling()
        finally:
            self._perform_final_cleanup()
