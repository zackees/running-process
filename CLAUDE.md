# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture Overview

This is a modern Python library for subprocess process management with the following core components:

- **RunningProcess**: Main class wrapping subprocess.Popen with enhanced features (output streaming, process tree management, timeout handling)
- **ProcessOutputReader**: Dedicated threaded reader that drains process stdout/stderr to prevent blocking
- **RunningProcessManager**: Thread-safe singleton registry for tracking active processes and debugging
- **OutputFormatter**: Protocol for transforming process output (with NullOutputFormatter as default)
- **process_utils**: Utilities for process tree operations (requires optional psutil dependency)

The package follows a layered design where RunningProcess orchestrates ProcessOutputReader and integrates with RunningProcessManager for lifecycle management.

## Development Commands

**Testing:**
```bash
./test                    # Run all tests with pytest
uv run pytest -n auto tests -v --durations=0  # Direct pytest command
```

**Linting:**
```bash
./lint                    # Run complete linting suite (ruff, black, isort, pyright)
uv run ruff check --fix src tests  # Just ruff linting
uv run black src tests    # Code formatting
uv run pyright src tests  # Type checking
```

**Environment Setup:**
```bash
. ./activate.sh          # Activate development environment (requires git-bash on Windows)
```

## Import Resolution Guidelines

**Use fully qualified absolute imports for all module resolution**:
- Use `from package.module import Class` instead of relative imports `from .module import Class`
- This ensures clear import paths and avoids ambiguity
- Example: `from running_process.output_formatter import OutputFormatter` not `from .output_formatter import OutputFormatter`
- Apply this rule to ALL imports within the package, including internal module imports

## Subprocess Command Guidelines

**Never use str.join() to convert subprocess command lists to command strings**:
- Use `subprocess.list2cmdline()` instead of `str.join()` for proper shell escaping
- This ensures proper handling of arguments containing spaces, quotes, and special characters
- Example: `subprocess.list2cmdline(command)` not `' '.join(command)`
- This prevents command injection vulnerabilities and ensures cross-platform compatibility

## Testing Framework Guidelines

**Use unittest framework for all test code**:
- Write tests using Python's standard `unittest` framework (TestCase, setUp, tearDown, etc.)
- Use `unittest` assertions (assertEqual, assertTrue, assertRaises, etc.) instead of pytest-specific features
- Pytest is used only as the test runner, not for test features
- Avoid pytest-specific decorators, fixtures, and assertion styles
- This ensures tests are portable and work with any test runner

## Code Quality Notes

- **Complex Functions**: Three functions have high complexity and should be refactored if modified:
  - `ProcessOutputReader.run()` (complexity 12)
  - `RunningProcess.get_next_line()` (complexity 16)
  - `RunningProcess.wait()` (complexity 20)
- **Print Statements**: Console output via print() is intentional for CLI functionality
- **Exception Handling**: Broad exception handling is acceptable for process cleanup/recovery scenarios
- **Cross-Platform**: Code must work on Windows (MSYS), macOS, and Linux