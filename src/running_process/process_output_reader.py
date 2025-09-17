"""Process output reader module.

This module contains the ProcessOutputReader class for handling subprocess output
in a dedicated thread to prevent blocking issues.
"""

import _thread
import logging
import threading
import time
import traceback
import warnings
from collections.abc import Callable
from subprocess import Popen
from typing import Any

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
        proc: Popen[Any],
        shutdown: threading.Event,
        output_formatter: OutputFormatter | None,
        on_output: Callable[[str | EndOfStream], None],
        on_end: Callable[[], None],
    ) -> None:
        output_formatter = output_formatter or NullOutputFormatter()
        self._proc = proc
        self._shutdown = shutdown
        self._output_formatter = output_formatter
        self._on_output = on_output
        self._on_end = on_end
        self.last_stdout_ts: float | None = None
        self._eos_emitted: bool = False

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
        assert self._proc.stdout is not None

        for line in self._proc.stdout:
            self.last_stdout_ts = time.time()
            if self._shutdown.is_set():
                break

            line_stripped = line.rstrip()
            if not line_stripped:
                continue

            transformed_line = self._output_formatter.transform(line_stripped)
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
        if self._proc.stdout and not self._proc.stdout.closed:
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
