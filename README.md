# running-process

[![Linting](../../actions/workflows/lint.yml/badge.svg)](../../actions/workflows/lint.yml)
[![MacOS_Tests](../../actions/workflows/push_macos.yml/badge.svg)](../../actions/workflows/push_macos.yml)
[![Ubuntu_Tests](../../actions/workflows/push_ubuntu.yml/badge.svg)](../../actions/workflows/push_ubuntu.yml)
[![Win_Tests](../../actions/workflows/push_win.yml/badge.svg)](../../actions/workflows/push_win.yml)

A modern subprocess.Popen wrapper with improved process management, real-time output streaming, and enhanced lifecycle control.

## Features

- **Real-time Output Streaming**: Stream process output via queues with customizable formatting
- **Thread-safe Process Management**: Centralized registry for tracking and debugging active processes
- **Enhanced Timeout Handling**: Optional stack trace dumping for debugging hanging processes
- **Process Tree Termination**: Kill entire process trees including child processes (requires psutil)
- **Cross-platform Support**: Works on Windows (MSYS), macOS, and Linux
- **Flexible Output Formatting**: Protocol-based output transformation with built-in formatters
- **Iterator Interface**: Context-managed line-by-line iteration over process output

## Quick Start

### Basic Usage

```python
from running_process import RunningProcess

# Simple command execution with real-time output
process = RunningProcess(["echo", "Hello World"])
for line in process:
    print(f"Output: {line}")

# Check exit code
if process.wait() != 0:
    print("Command failed!")
```

### Advanced Features

```python
from running_process import RunningProcess
from pathlib import Path

# Advanced configuration
process = RunningProcess(
    command=["python", "long_script.py"],
    cwd=Path("./scripts"),
    timeout=300,  # 5 minute timeout
    enable_stack_trace=True,  # Debug hanging processes
    check=True,  # Raise exception on non-zero exit
)

# Process output as it arrives
while process.is_running():
    try:
        line = process.get_next_line(timeout=1.0)
        print(f"[{process.elapsed_time:.1f}s] {line}")
    except TimeoutError:
        print("No output for 1 second...")
        continue

# Wait for completion
exit_code = process.wait()
```

### Output Formatting

```python
from running_process import RunningProcess
from running_process.output_formatter import create_sketch_path_formatter

# Use built-in path formatter
formatter = create_sketch_path_formatter("MyProject")
process = RunningProcess(
    ["gcc", "-v", "main.c"],
    output_formatter=formatter
)

# Implement custom formatter
class TimestampFormatter:
    def begin(self): pass
    def end(self): pass

    def transform(self, line: str) -> str:
        from datetime import datetime
        timestamp = datetime.now().strftime("%H:%M:%S")
        return f"[{timestamp}] {line}"

process = RunningProcess(["make"], output_formatter=TimestampFormatter())
```

### Process Management

```python
from running_process import RunningProcessManager

# Access the global process registry
manager = RunningProcessManager.get_instance()

# List all active processes
for proc_id, process in manager.get_all_processes():
    print(f"Process {proc_id}: {process.command_str}")

# Clean up finished processes
manager.cleanup_finished_processes()
```

## Installation

```bash
pip install running-process
```

### Optional Dependencies

For process tree termination support:
```bash
pip install running-process[psutil]
# or
pip install psutil
```

## Architecture

The library follows a layered design with these core components:

- **RunningProcess**: Main class wrapping subprocess.Popen with enhanced features
- **ProcessOutputReader**: Dedicated threaded reader that drains process stdout/stderr
- **RunningProcessManager**: Thread-safe singleton registry for tracking active processes
- **OutputFormatter**: Protocol for transforming process output with built-in implementations
- **process_utils**: Utilities for process tree operations (requires optional psutil dependency)

## Development

### Setup

```bash
# Clone the repository
git clone https://github.com/yourusername/running-process.git
cd running-process

# Activate development environment (requires git-bash on Windows)
. ./activate.sh
```

### Testing

```bash
# Run all tests
./test

# Run with coverage
uv run pytest --cov=running_process tests/
```

### Linting

```bash
# Run complete linting suite
./lint

# Individual tools
uv run ruff check --fix src tests
uv run black src tests
uv run pyright src tests
```

## API Reference

### RunningProcess

The main class for managing subprocess execution:

```python
class RunningProcess:
    def __init__(
        self,
        command: str | list[str],
        cwd: Path | None = None,
        check: bool = False,
        auto_run: bool = True,
        shell: bool | None = None,
        timeout: int | None = None,
        enable_stack_trace: bool = False,
        on_complete: Callable[[], None] | None = None,
        output_formatter: OutputFormatter | None = None,
    ) -> None: ...

    def get_next_line(self, timeout: float | None = None) -> str | EndOfStream: ...
    def wait(self, timeout: float | None = None) -> int: ...
    def kill(self) -> None: ...
    def is_running(self) -> bool: ...
    def drain_stdout(self) -> list[str]: ...
```

### Key Methods

- `get_next_line(timeout)`: Get the next line of output with optional timeout
- `wait(timeout)`: Wait for process completion, returns exit code
- `kill()`: Terminate the process (and process tree if psutil available)
- `is_running()`: Check if process is still executing
- `drain_stdout()`: Get all currently available output lines

### OutputFormatter Protocol

```python
class OutputFormatter(Protocol):
    def begin(self) -> None: ...
    def transform(self, line: str) -> str: ...
    def end(self) -> None: ...
```

## License

BSD 3-Clause License

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes following the existing code style
4. Run tests and linting: `./test && ./lint`
5. Submit a pull request

For bug reports and feature requests, please use the [GitHub Issues](https://github.com/yourusername/running-process/issues) page.