"""Enhanced subprocess execution with timeout protection, output streaming, and process tree management.

## Basic Usage

### Simple Command Execution
```python
# List command - automatically starts execution
process = RunningProcess(["echo", "Hello World"])
exit_code = process.wait()
print(process.stdout)  # "Hello World"

# Manual start control
process = RunningProcess(["ls", "-la"], auto_run=False)
process.start()
exit_code = process.wait()
```

### Shell Commands
```python
# String commands require shell=True
process = RunningProcess("echo 'Hello World'", shell=True)
exit_code = process.wait()
```

### Live Output Streaming
```python
# Line-by-line output processing
process = RunningProcess(["python", "long_script.py"])
for line in process.line_iter(timeout=1.0):
    print(f"Output: {line}")

# Non-blocking polling
while not process.finished:
    line = process.get_next_line_non_blocking()
    if line:
        handle_output(line)
    time.sleep(0.1)

# Echo output as it arrives
process.wait(echo=True)  # Prints to console
```

### Timeout Protection with Custom Handlers
```python
def debug_timeout(process_info):
    print(f"Process {process_info.pid} timed out after {process_info.duration}s")
    # Custom debugging logic (GDB, pstack, profiling, etc.)
    subprocess.run(["gdb", "-batch", "-ex", f"attach {process_info.pid}",
                   "-ex", "bt", "-ex", "detach"])

process = RunningProcess(
    command=["make", "build"],
    timeout=300,  # 5 minute timeout
    on_timeout=debug_timeout,
    on_complete=lambda: print("Build completed!")
)
```

### Output Formatting
```python
from running_process import TimeDeltaFormatter

# Add timestamps to output lines
process = RunningProcess(
    command=["pytest", "tests/"],
    output_formatter=TimeDeltaFormatter(),  # "[1.23] test output"
    timeout=300
)
```

### PTY Support for Interactive Commands
```python
# Use pseudo-terminal for commands requiring interactive terminal
process = RunningProcess(
    command=["ssh", "user@host", "ls"],
    use_pty=True,  # Enables PTY mode
    timeout=30
)
exit_code = process.wait()

# PTY automatically filters ANSI escape sequences
# and handles commands that behave differently in terminals
```

### Simple subprocess.run() Replacement
```python
# Drop-in replacement for subprocess.run()
result = subprocess_run(
    command=["git", "status"],
    cwd=Path("/project"),
    timeout=10,
    check=True
)
print(result.stdout)
print(result.returncode)
```

## Key Features

- **Non-blocking output**: Threaded output reader prevents process hanging
- **Timeout protection**: Global and per-operation timeouts with custom handlers
- **Process tree management**: Automatically kills child processes to prevent orphans
- **Output streaming**: Real-time line iteration and non-blocking access
- **PTY support**: Pseudo-terminal support for interactive commands (requires winpty on Windows)
- **ANSI filtering**: Automatic filtering of escape sequences in PTY mode
- **Flexible callbacks**: Custom timeout and completion handlers
- **Cross-platform**: Works on Windows, macOS, and Linux
- **Thread-safe**: Safe for concurrent use with proper synchronization
"""

# pyright: reportUnknownMemberType=false, reportMissingParameterType=false
import _thread
import contextlib
import logging
import os
import queue
import signal
import subprocess
import sys
import threading
import time
import warnings
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from queue import Queue
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from running_process.pty import PtyProcessProtocol


from running_process.line_iterator import _RunningProcessLineIterator
from running_process.output_formatter import NullOutputFormatter, OutputFormatter
from running_process.process_output_reader import EndOfStream, ProcessOutputReader
from running_process.process_utils import kill_process_tree
from running_process.process_watcher import ProcessWatcher
from running_process.pty import Pty, PtyNotAvailableError
from running_process.running_process_manager import RunningProcessManagerSingleton
from running_process.subprocess_runner import execute_subprocess_run

# Create module-level logger
logger = logging.getLogger(__name__)


@dataclass
class ProcessInfo:
    """Information about a process passed to timeout callbacks."""

    pid: int
    command: str | list[str]
    duration: float


# Type alias for echo callbacks
EchoCallback = Callable[[str], None]


class EchoCallbackNull:
    """Null object implementation of EchoCallback that discards all output."""

    def __call__(self, line: str) -> None:
        """Discard the input line without doing anything."""


def _normalize_echo_callback(echo: bool | EchoCallback) -> EchoCallback:
    """Normalize echo parameter to a callback function.

    Args:
        echo: Either a boolean or a callback function.
              True converts to print function, False to EchoCallbackNull.

    Returns:
        Callback function that handles output lines.
    """
    if echo is True:
        return print
    if echo is False:
        return EchoCallbackNull()
    if callable(echo):
        return echo

    error_msg = f"echo must be bool or callable, got {type(echo).__name__}"
    raise TypeError(error_msg)


# Console UTF-8 configuration is now handled globally in ci/__init__.py


class RunningProcess:
    """
    A class to manage and stream output from a running subprocess.

    This class provides functionality to execute shell commands, stream their output
    in real-time via a queue, and control the subprocess execution. It merges stderr
    into stdout and provides thread-safe access to process output.

    Key features:
    - Real-time output streaming via queue
    - Thread-safe output consumption
    - Timeout protection with optional stack traces
    - Echo mode for immediate output printing
    - Process tree termination support
    """

    def __init__(
        self,
        command: str | list[str],
        cwd: Path | None = None,
        check: bool = False,
        auto_run: bool = True,
        shell: bool | None = None,
        timeout: int | None = None,  # None means no global timeout
        on_timeout: Callable[[ProcessInfo], None] | None = None,  # Callback to execute on timeout
        on_complete: Callable[[], None] | None = None,  # Callback to execute when process completes
        output_formatter: OutputFormatter | None = None,
        use_pty: bool = False,  # Use pseudo-terminal for process execution
    ) -> None:
        """
        Initialize the RunningProcess instance.

        Note: stderr is automatically merged into stdout for unified output handling.

        Args:
            command: The command to execute as string or list of arguments.
            cwd: Working directory to execute the command in.
            check: If True, raise CalledProcessError if command returns non-zero exit code.
            auto_run: If True, automatically start the command when instance is created.
            shell: Shell execution mode. None auto-detects based on command type.
            timeout: Global timeout in seconds for process execution. None disables timeout.
            on_timeout: Callback function executed when process times out. Receives ProcessInfo.
            on_complete: Callback function executed when process completes normally.
            output_formatter: Optional formatter for transforming output lines.
            use_pty: If True, use pseudo-terminal for process execution (supports interactive commands).
        """
        # Validate command/shell combination
        if isinstance(command, str) and shell is False:
            error_message = "String commands require shell=True. Use shell=True or provide command as list[str]."
            raise ValueError(error_message)

        if shell is None:
            # Default: use shell only when given a string, or when a list includes shell metachars
            if isinstance(command, str):
                shell = True
            else:  # must be list[str] since command: str | list[str]
                shell_meta = {"&&", "||", "|", ";", ">", "<", "2>", "&"}
                shell = any(part in shell_meta for part in command)

        # Validate shell metacharacters when shell=False
        if shell is False and isinstance(command, list):
            shell_meta = {"&&", "||", "|", ";", ">", "<", "2>", "&"}
            found_meta = [part for part in command if part in shell_meta]
            if found_meta:
                error_message = (
                    f"Shell metacharacters {found_meta} found in command but shell=False. "
                    f"Either set shell=True or remove shell metacharacters from the command."
                )
                raise ValueError(error_message)
        self.command = command
        self.shell: bool = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.output_queue: Queue[str | EndOfStream] = Queue()
        self.accumulated_output: list[str] = []  # Store all output for later retrieval
        self.proc: subprocess.Popen[Any] | PtyProcessProtocol | None = None
        self.check = check
        self.auto_run = auto_run
        self.timeout = timeout
        self.on_timeout = on_timeout
        self.on_complete = on_complete
        # Always keep a non-None formatter
        self.output_formatter = output_formatter if output_formatter is not None else NullOutputFormatter()
        self.reader_thread: threading.Thread | None = None
        self.watcher_thread: threading.Thread | None = None
        self.shutdown: threading.Event = threading.Event()
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._time_last_stdout_line: float | None = None
        self._termination_notified: bool = False
        # PTY support fields
        self.use_pty = use_pty and self._pty_available()
        self._pty_proc: Any = None  # winpty.PtyProcess or None
        self._pty_master_fd: int | None = None  # Unix PTY master file descriptor
        if auto_run:
            self.start()

    def _pty_available(self) -> bool:
        """Check PTY support for current platform."""
        return Pty.is_available()

    def get_command_str(self) -> str:
        if isinstance(self.command, list):
            return subprocess.list2cmdline(self.command)
        return self.command

    def _create_process_info(self) -> ProcessInfo:
        """Create ProcessInfo for timeout callbacks."""
        if self.proc is None or self._start_time is None:
            duration = 0.0
            pid = 0
        else:
            duration = time.time() - self._start_time
            pid = self.proc.pid

        return ProcessInfo(pid=pid, command=self.command, duration=duration)

    def time_last_stdout_line(self) -> float | None:
        return self._time_last_stdout_line

    def _handle_timeout(self, timeout: float, echo_callback: EchoCallback) -> None:
        """Handle process timeout with optional callback and cleanup."""
        cmd_str = self.get_command_str()

        # Drain any remaining output before killing
        remaining_lines = self.drain_stdout()
        for line in remaining_lines:
            echo_callback(line)
        if remaining_lines:
            echo_callback(f"[Drained {len(remaining_lines)} final lines before timeout]")

        # Execute user-provided timeout callback if available
        if self.on_timeout is not None:
            try:
                process_info = self._create_process_info()
                self.on_timeout(process_info)
            except (AttributeError, TypeError, ValueError, RuntimeError) as e:
                logger.warning("Timeout callback failed: %s", e)

        logger.warning("Killing timed out process: %s", cmd_str)
        self.kill()
        timeout_error_msg = f"Process timed out after {timeout} seconds: {cmd_str}"
        raise TimeoutError(timeout_error_msg)

    def drain_stdout(self) -> list[str]:
        """
        Drain all currently pending stdout lines without blocking.

        Consumes all available lines from the output queue until either the queue
        is empty or EndOfStream is encountered. The EndOfStream sentinel is preserved
        by get_next_line() for other callers.

        Returns:
            List of output lines that were available. Empty list if no output pending.
        """
        lines: list[str] = []

        while True:
            try:
                line = self.get_next_line(timeout=0)
                if isinstance(line, EndOfStream):
                    break  # get_next_line already handled EndOfStream preservation
                lines.append(line)
            except TimeoutError:
                break  # Queue is empty

        return lines

    def has_pending_output(self) -> bool:
        """
        Check if there are pending output lines without consuming them.

        Returns:
            True if output lines are available in the queue, False otherwise.
            Returns False if only EndOfStream sentinel is present.
        """
        try:
            with self.output_queue.mutex:
                if len(self.output_queue.queue) == 0:
                    return False
                # If the only item is EndOfStream, no actual output is pending
                return not (len(self.output_queue.queue) == 1 and isinstance(self.output_queue.queue[0], EndOfStream))
        except (AttributeError, TypeError):
            return False

    def _prepare_command(self) -> str | list[str]:
        """Prepare the command for subprocess.Popen based on shell settings."""
        if self.shell and isinstance(self.command, list):
            # Convert list to a single shell string with proper quoting
            return subprocess.list2cmdline(self.command)
        return self.command

    def _create_process(self) -> None:
        """Create the subprocess with proper configuration."""
        if self.use_pty:
            self._create_process_with_pty()
        else:
            self._create_process_with_pipe()

        # Track start time after process is successfully created
        # This excludes process creation overhead from timing measurements
        self._start_time = time.time()

    def _create_process_with_pipe(self) -> None:
        """Create subprocess with standard pipes."""
        popen_command = self._prepare_command()

        # Force unbuffered output for Python subprocesses to prevent stdout buffering
        # when output is piped to another process (prevents multi-second delays)
        env = os.environ.copy()
        env["PYTHONUNBUFFERED"] = "1"

        self.proc = subprocess.Popen(  # noqa: S603
            popen_command,
            shell=self.shell,
            cwd=self.cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,  # Merge stderr into stdout
            text=True,  # Use text mode
            encoding="utf-8",  # Explicitly use UTF-8
            errors="replace",  # Replace invalid chars instead of failing
            bufsize=1,  # Line-buffered for real-time output
            env=env,
        )

    def _create_process_with_pty(self) -> None:
        """Create subprocess with PTY allocation using unified PTY wrapper."""
        try:
            # Force unbuffered output for Python subprocesses
            env = os.environ.copy()
            env["PYTHONUNBUFFERED"] = "1"

            pty_wrapper = Pty()
            pty_process = pty_wrapper.spawn_process(
                command=self.command,
                cwd=self.cwd,
                env=env,
                shell=self.shell,
            )
            self.proc = pty_process  # type: ignore[assignment]
            self._pty_proc = pty_process
        except PtyNotAvailableError:
            # Fall back to regular pipe-based process if PTY is not available
            logger.warning("PTY requested but not available, falling back to pipes")
            self.use_pty = False
            self._create_process_with_pipe()

    def _register_with_manager(self) -> None:
        """Register this process with the global process manager."""
        try:
            RunningProcessManagerSingleton.register(self)
        except (AttributeError, TypeError, RuntimeError) as e:
            register_error_msg = f"RunningProcessManager.register failed: {e}"
            warnings.warn(register_error_msg, stacklevel=2)

    def _create_output_callbacks(self) -> tuple[Callable[[str | EndOfStream], None], Callable[[], None]]:
        """Create the callback functions for output handling."""

        def _on_reader_end() -> None:
            # Set end time when stdout pumper finishes; captures completion time of useful output
            if self._end_time is None:
                self._end_time = time.time()
            # Unregister when stdout is fully drained
            try:
                self._notify_terminated()
            except (AttributeError, TypeError, RuntimeError) as e:
                notify_error_msg = f"RunningProcess termination notify (drain) failed: {e}"
                warnings.warn(notify_error_msg, stacklevel=2)

        def _on_output(item: str | EndOfStream) -> None:
            # Forward to queue and capture text lines for accumulated output
            if isinstance(item, EndOfStream):
                self.output_queue.put(item)
            else:
                # Track time of last stdout line observed
                self._time_last_stdout_line = time.time()
                self.output_queue.put(item)
                self.accumulated_output.append(item)

        return _on_output, _on_reader_end

    def _start_reader_thread(self, on_output: Callable[[str | EndOfStream], None], on_end: Callable[[], None]) -> None:
        """Start the output reader thread."""
        assert self.proc is not None

        reader = ProcessOutputReader(
            proc=self.proc,
            shutdown=self.shutdown,
            output_formatter=self.output_formatter,
            on_output=on_output,
            on_end=on_end,
            use_pty=self.use_pty,
            pty_proc=self._pty_proc,
            pty_master_fd=self._pty_master_fd,
        )

        # Start output reader thread
        self.reader_thread = threading.Thread(target=reader.run, daemon=True)
        self.reader_thread.start()

    def _start_watcher_thread(self) -> None:
        """Start the process watcher thread."""
        self._watcher = ProcessWatcher(self)
        self._watcher.start()
        self.watcher_thread = self._watcher.thread

    def start(self) -> None:
        """
        Execute the command and stream its output to the queue.

        Raises:
            subprocess.CalledProcessError: If the command returns a non-zero exit code.
        """
        assert self.proc is None

        # Create and configure the subprocess
        self._create_process()

        # Register with global process manager
        self._register_with_manager()

        # Setup output handling
        on_output, on_end = self._create_output_callbacks()

        # Start monitoring threads
        self._start_reader_thread(on_output, on_end)
        self._start_watcher_thread()

    def _handle_immediate_timeout(self) -> str | EndOfStream:
        """Handle timeout=0 case for immediate non-blocking access."""
        # Peek EOS without consuming
        with self.output_queue.mutex:
            if len(self.output_queue.queue) > 0:
                head = self.output_queue.queue[0]
                if isinstance(head, EndOfStream):
                    return EndOfStream()
        # Try immediate get
        try:
            item_nb: str | EndOfStream = self.output_queue.get_nowait()
            if isinstance(item_nb, EndOfStream):
                with self.output_queue.mutex:
                    self.output_queue.queue.appendleft(item_nb)
                return EndOfStream()
        except queue.Empty:
            if self.finished:
                return EndOfStream()
            immediate_timeout_msg = "Timeout after 0 seconds"
            raise TimeoutError(immediate_timeout_msg) from None
        else:
            return item_nb

    def _peek_for_end_of_stream(self) -> bool:
        """Check if EndOfStream is at the front of the queue."""
        with self.output_queue.mutex:
            if len(self.output_queue.queue) > 0:
                head = self.output_queue.queue[0]
                return isinstance(head, EndOfStream)
        return False

    def _get_item_from_queue(self) -> str | EndOfStream | None:
        """Try to get an item from the queue, returning None if empty."""
        try:
            # Safe to pop now; head is not EndOfStream
            item: str | EndOfStream = self.output_queue.get(timeout=0.1)
            if isinstance(item, EndOfStream):
                # In rare race conditions, EndOfStream could appear after peek; put back for other callers
                with self.output_queue.mutex:
                    self.output_queue.queue.appendleft(item)
                return EndOfStream()
        except queue.Empty:
            if self.finished:
                return EndOfStream()
            return None
        else:
            return item

    def _check_timeout_expired(self, expired_time: float | None, timeout: float | None) -> None:
        """Check if timeout has expired and raise TimeoutError if so."""
        if expired_time is not None and time.time() > expired_time:
            timeout_msg = f"Timeout after {timeout} seconds"
            raise TimeoutError(timeout_msg)

    def _wait_for_output_or_completion(self) -> bool:
        """Wait briefly for output or process completion. Returns True if should continue waiting."""
        if self.output_queue.empty():
            time.sleep(0.01)
            return not (self.finished and self.output_queue.empty())  # Stop waiting if process finished
        return False  # Queue has items, stop waiting

    def get_next_line(self, timeout: float | None = None) -> str | EndOfStream:
        """
        Get the next line of output from the process.

        Args:
            timeout: How long to wait for the next line in seconds.
                    None means wait forever, 0 means don't wait.

        Returns:
            str: The next line of output if available.
            EndOfStream: Process has finished, no more output will be available.

        Raises:
            TimeoutError: If timeout is reached before a line becomes available.
        """
        assert self.proc is not None

        # Fast non-blocking path: honor timeout==0 by peeking before raising
        if timeout == 0:
            return self._handle_immediate_timeout()

        expired_time = time.time() + timeout if timeout is not None else None

        while True:
            self._check_timeout_expired(expired_time, timeout)

            # Check if EndOfStream is at the front
            if self._peek_for_end_of_stream():
                return EndOfStream()

            # Wait for output or completion
            if self._wait_for_output_or_completion():
                continue

            # Try to get an item from the queue
            item = self._get_item_from_queue()
            if item is not None:
                return item
            # Continue loop if item is None (queue was empty)

    def get_next_line_non_blocking(self) -> str | None | EndOfStream:
        """
        Get the next line of output from the process without blocking.

        Returns:
            str: Next line of output if available
            None: No output available right now (should continue polling)
            EndOfStream: Process has finished, no more output will be available
        """
        try:
            line: str | EndOfStream = self.get_next_line(timeout=0)
        except TimeoutError:
            # Check if process finished while we were waiting
            if self.finished:
                return EndOfStream()
            return None
        else:
            return line  # get_next_line already handled EndOfStream preservation

    def poll(self) -> int | None:
        """
        Check the return code of the process.
        """
        if self.proc is None:
            return None
        rc = self.proc.poll()
        if rc is not None:
            # Ensure unregistration only happens once
            try:
                self._notify_terminated()
            except (AttributeError, TypeError, RuntimeError) as e:
                poll_error_msg = f"RunningProcess termination notify (poll) failed: {e}"
                warnings.warn(poll_error_msg, stacklevel=2)
        return rc

    @property
    def finished(self) -> bool:
        return self.poll() is not None

    def _echo_output_lines(self, lines: list[str], echo_callback: EchoCallback) -> None:
        """Echo output lines using the provided callback."""
        for line in lines:
            echo_callback(line)
        # Additional flush for Unix systems for better performance when using print
        if echo_callback is print and os.name != "nt":
            sys.stdout.flush()

    def _check_process_timeout(
        self, effective_timeout: float | None, start_time: float, echo_callback: EchoCallback
    ) -> None:
        """Check if process has timed out and handle it."""
        if effective_timeout is not None and (time.time() - start_time) > effective_timeout:
            self._handle_timeout(effective_timeout, echo_callback=echo_callback)

    def _handle_echo_output(self, echo_callback: EchoCallback) -> bool:
        """Handle echoing output. Returns True if output was found and echoed."""
        lines = self.drain_stdout()
        if lines:
            self._echo_output_lines(lines, echo_callback)
            return True
        return False

    def _wait_for_process_completion(
        self, effective_timeout: float | None, echo_callback: EchoCallback, start_time: float
    ) -> None:
        """Wait for process to complete with timeout and echo handling."""
        while self.poll() is None:
            self._check_process_timeout(effective_timeout, start_time, echo_callback)

            # Echo: drain all available output, then sleep
            if self._handle_echo_output(echo_callback):
                continue  # Check for more output immediately

            time.sleep(0.01)  # Check every 10ms

    def _handle_process_completion_echo(self, echo_callback: EchoCallback) -> None:
        """Handle echoing output after process completion."""
        # Process completed - drain any remaining output
        remaining_lines = self.drain_stdout()
        for line in remaining_lines:
            echo_callback(line)
        if remaining_lines:
            echo_callback(f"[Drained {len(remaining_lines)} final lines after completion]")

    def _handle_keyboard_interrupt_detection(self, rtn: int) -> bool:
        """Check for keyboard interrupt and handle it. Returns True if was keyboard interrupt."""
        is_keyboard_interrupt = rtn in (-11, 3221225786)
        if is_keyboard_interrupt:
            logger.info("Keyboard interrupt detected, interrupting main thread")
            _thread.interrupt_main()
        return is_keyboard_interrupt

    def _cleanup_reader_thread(self) -> None:
        """Clean up the reader thread with timeout."""
        if self.reader_thread is not None:
            self.reader_thread.join(timeout=0.05)  # 50ms should be plenty for thread cleanup
            if self.reader_thread.is_alive():
                # Reader thread didn't finish, force shutdown
                self.shutdown.set()
                self.reader_thread.join(timeout=0.05)  # 50ms for forced shutdown

    def _execute_completion_callback(self) -> None:
        """Execute the completion callback if provided."""
        if self.on_complete is not None:
            try:
                self.on_complete()
            except (AttributeError, TypeError, RuntimeError) as e:
                logger.info("Warning: on_complete callback failed: %s", e)

    def _finalize_wait(self, echo_callback: EchoCallback) -> None:
        """Finalize the wait process with cleanup and notifications."""
        # Final drain after reader threads shut down - catch any remaining queued output
        final_lines = self.drain_stdout()
        for line in final_lines:
            echo_callback(line)

        # Execute completion callback if provided
        self._execute_completion_callback()

        # Unregister from global process manager on normal completion
        try:
            self._notify_terminated()
        except (AttributeError, TypeError, RuntimeError) as e:
            wait_error_msg = f"RunningProcess termination notify (wait) failed: {e}"
            warnings.warn(wait_error_msg, stacklevel=2)

    def _validate_process_started(self) -> None:
        """Validate that the process has been started."""
        if self.proc is None:
            error_message = "Process is not running."
            raise ValueError(error_message)

    def _determine_effective_timeout(self, timeout: float | None) -> float | None:
        """Determine the effective timeout to use."""
        return timeout if timeout is not None else self.timeout

    def _get_process_return_code(self) -> int:
        """Get the process return code after completion."""
        assert self.proc is not None  # For type checker
        rtn = self.proc.returncode
        assert rtn is not None  # Process has completed, so returncode exists
        return rtn

    def _finalize_process_timing(self) -> None:
        """Record end time if not already set by output reader."""
        if self._end_time is None:
            self._end_time = time.time()

    def wait(self, echo: bool | EchoCallback = False, timeout: float | None = None) -> int:
        """
        Wait for the process to complete with timeout protection.

        When echo=True, continuously drains and prints stdout lines while waiting.
        When echo is a callback, uses that function to handle output lines.
        Performs final output drain after process completion and thread cleanup.

        Args:
            echo: If True, continuously print stdout lines as they become available.
                  If callable, use that function to handle output lines.
                  If False, no output echoing.
            timeout: Overall timeout in seconds. If None, uses instance timeout.
                    If both are None, waits indefinitely.

        Returns:
            Process exit code.

        Raises:
            ValueError: If the process hasn't been started.
            TimeoutError: If the process exceeds the timeout duration.
            TypeError: If echo is not bool or callable.
        """
        self._validate_process_started()
        effective_timeout = self._determine_effective_timeout(timeout)
        echo_callback = _normalize_echo_callback(echo)
        start_time = time.time()

        # Wait for process completion
        self._wait_for_process_completion(effective_timeout, echo_callback, start_time)

        # Handle post-completion echoing
        self._handle_process_completion_echo(echo_callback)

        # Get return code and handle special cases
        rtn = self._get_process_return_code()
        if self._handle_keyboard_interrupt_detection(rtn):
            return 1

        # Finalize timing and cleanup
        self._finalize_process_timing()
        self._cleanup_reader_thread()
        self._finalize_wait(echo_callback)

        return rtn

    def kill(self) -> None:  # noqa: C901
        """
        Immediately terminate the process and all child processes.

        Signals reader threads to shutdown, kills the entire process tree to prevent
        orphaned processes, and waits for thread cleanup. Safe to call multiple times.

        Note: Does not raise if process is already terminated or was never started.
        """
        if self.proc is None:
            return

        # Record end time when killed (only if not already set by output reader)
        if self._end_time is None:
            self._end_time = time.time()

        # Signal reader thread to stop
        self.shutdown.set()

        # PTY-specific cleanup must happen before killing process tree
        if self.use_pty:
            logger.debug("Cleaning up PTY resources before process termination")
            if sys.platform == "win32" and self._pty_proc:
                try:
                    self._pty_proc.kill(signal.SIGTERM)
                    logger.debug("Killed winpty process")
                except (OSError, ValueError, RuntimeError) as e:
                    logger.warning("Failed to kill winpty process: %s", e)
            elif self._pty_master_fd is not None:
                try:
                    os.close(self._pty_master_fd)
                    self._pty_master_fd = None
                    logger.debug("Closed Unix PTY master fd")
                except (OSError, ValueError) as e:
                    logger.warning("Failed to close PTY master fd: %s", e)

        # Kill the entire process tree (parent + all children)
        # This prevents orphaned clang++ processes from hanging the system
        try:
            kill_process_tree(self.proc.pid)
        except KeyboardInterrupt:
            logger.info("Keyboard interrupt detected in kill(), interrupting main thread")
            _thread.interrupt_main()
            # Extra cleanup for PTY on KeyboardInterrupt
            if self.use_pty:
                logger.warning("KeyboardInterrupt during PTY process kill - forcing cleanup")
            try:
                self.proc.kill()
            except (ProcessLookupError, PermissionError, OSError, ValueError) as e:
                logger.info("Warning: Failed to kill process tree for %s: %s", self.proc.pid, e)
            raise
        except (OSError, subprocess.SubprocessError, AttributeError, ImportError) as e:
            # Fallback to simple kill if tree kill fails
            logger.info("Warning: Failed to kill process tree for %s: %s", self.proc.pid, e)
            with contextlib.suppress(ProcessLookupError, PermissionError, OSError, ValueError):
                self.proc.kill()  # Process might already be dead

        # Wait for reader thread to finish
        if self.reader_thread is not None:
            self.reader_thread.join(timeout=0.05)  # 50ms should be plenty for cleanup

        # Ensure unregistration even on forced kill
        try:
            RunningProcessManagerSingleton.unregister(self)
        except (AttributeError, TypeError, RuntimeError) as e:
            kill_error_msg = f"RunningProcessManager.unregister (kill) failed: {e}"
            warnings.warn(kill_error_msg, stacklevel=2)

    def _notify_terminated(self) -> None:
        """Idempotent notification that the process has terminated.

        Ensures unregister is called only once across multiple termination paths
        (poll, wait, stdout drain, watcher thread) and records end time when
        available.
        """
        if self._termination_notified:
            return
        self._termination_notified = True

        # Record end time only if not already set
        if self._end_time is None:
            self._end_time = time.time()

        try:
            RunningProcessManagerSingleton.unregister(self)
        except (AttributeError, TypeError, RuntimeError) as e:
            notify_unreg_error_msg = f"RunningProcessManager.unregister notify failed: {e}"
            warnings.warn(notify_unreg_error_msg, stacklevel=2)

    def terminate(self) -> None:
        """
        Gracefully terminate the process with SIGTERM.

        Raises:
            ValueError: If the process hasn't been started.
        """
        if self.proc is None:
            error_message = "Process is not running."
            raise ValueError(error_message)
        self.shutdown.set()
        self.proc.terminate()

    @property
    def returncode(self) -> int | None:
        if self.proc is None:
            return None
        return self.proc.returncode

    @property
    def start_time(self) -> float | None:
        """Get the process start time"""
        return self._start_time

    @property
    def end_time(self) -> float | None:
        """Get the process end time"""
        return self._end_time

    @property
    def duration(self) -> float | None:
        """Get the process duration in seconds, or None if not completed"""
        if self._start_time is None or self._end_time is None:
            return None
        return self._end_time - self._start_time

    @property
    def stdout(self) -> str:
        """
        Get the complete stdout output accumulated so far.

        Returns all output lines that have been processed by the reader thread,
        joined with newlines. Available even while process is still running.

        Returns:
            Complete stdout output as a string. Empty string if no output yet.
        """
        # Return accumulated output (available even if process is still running)
        return "\n".join(self.accumulated_output)

    def line_iter(self, timeout: float | None) -> _RunningProcessLineIterator:
        """Return a context-managed iterator over output lines.

        Args:
            timeout: Per-line timeout in seconds. None waits indefinitely for each line.

        Returns:
            A context-managed iterator yielding non-empty, transformed stdout lines.
        """
        return _RunningProcessLineIterator(self, timeout)

    @staticmethod
    def run(
        command: str | list[str],
        cwd: Path | None,
        check: bool,
        timeout: int,
        on_timeout: Callable[[ProcessInfo], None] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Public static accessor for subprocess.run() replacement.

        Execute a command with robust stdout handling, emulating subprocess.run().

        Uses RunningProcess as the backend to provide:
        - Continuous stdout streaming to prevent pipe blocking
        - Merged stderr into stdout for unified output
        - Timeout protection with optional stack trace dumping
        - Standard subprocess.CompletedProcess return value

        Args:
            command: Command to execute as string or list of arguments.
            cwd: Working directory for command execution. Required parameter.
            check: If True, raise CalledProcessError for non-zero exit codes.
            timeout: Maximum execution time in seconds.
            on_timeout: Callback function executed when process times out.

        Returns:
            CompletedProcess with combined stdout and process return code.
            stderr field is None since it's merged into stdout.

        Raises:
            RuntimeError: If process times out (wraps TimeoutError).
            CalledProcessError: If check=True and process exits with non-zero code.
        """
        return execute_subprocess_run(command, cwd, check, timeout, on_timeout)


# NOTE: RunningProcessManager and its singleton live in running_process_manager.py


def subprocess_run(
    command: str | list[str],
    cwd: Path | None,
    check: bool,
    timeout: int,
    on_timeout: Callable[[ProcessInfo], None] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Execute a command with robust stdout handling, emulating subprocess.run().

    This is a backward compatibility wrapper for the static method RunningProcess.run().

    Args:
        command: Command to execute as string or list of arguments.
        cwd: Working directory for command execution. Required parameter.
        check: If True, raise CalledProcessError for non-zero exit codes.
        timeout: Maximum execution time in seconds.
        on_timeout: Callback function executed when process times out.

    Returns:
        CompletedProcess with combined stdout and process return code.
        stderr field is None since it's merged into stdout.

    Raises:
        RuntimeError: If process times out (wraps TimeoutError).
        CalledProcessError: If check=True and process exits with non-zero code.
    """
    return RunningProcess.run(command, cwd, check, timeout, on_timeout)
