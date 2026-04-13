# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture Overview

A Rust-backed Python library (v3.0.15) for subprocess and PTY process management across Windows, macOS, and Linux.

### Layered Design

**Python layer** (`src/running_process/`) provides high-level APIs:
- **`RunningProcess`**: Pipe-backed subprocess wrapper with output streaming, process tree management, and timeout handling
- **`PseudoTerminalProcess`**: PTY-backed process wrapper with expect patterns, idle detection, and terminal input relay
- **`InteractiveProcess`**: Unified facade dispatching to either pipe or PTY backends
- **`ProcessOutputReader`**: Threaded reader draining stdout/stderr to prevent blocking
- **`RunningProcessManager`**: Thread-safe singleton registry for tracking active processes

**Rust layer** (`crates/`) provides native performance:
- **`running-process-core`**: OS-level subprocess abstraction (`NativeProcess` — pipe I/O, signaling, Job Objects/process groups). No PTY.
- **`running-process-py`**: PyO3 bindings. Contains `NativePtyProcess` (via `portable-pty` crate) alongside the pipe backend. Exposes a unified `PyNativeProcess` with `NativeProcessBackend` enum dispatching to either `NativeRunningProcess` or `NativePtyProcess`
- **`running-process-proto`**: Protobuf schema for daemon IPC (`daemon.proto`). Field numbers in `RequestType` enum match payload field numbers.
- **`running-process-daemon`**: Persistent daemon for process tracking (Tokio, SQLite registry, protobuf IPC)
- **`daemon-trampoline`**: Minimal daemon launcher binary

**Python-Rust bridge**: `running_process._native` module compiled via maturin. Python's `PseudoTerminalProcess.start()` calls `NativeProcess.for_pty()` which creates a `NativePtyProcess` on the Rust side.

### Test Binaries

`testbins/` contains Rust binaries used as test fixtures: `env-reporter`, `sleeper`, `spawner`.

## Development Commands

**Build (native extension):**
```bash
uv run build.py              # Dev wheel, reinstalls into venv (default)
uv run build.py --release    # Publish-grade wheels in dist/
```

**Testing:**
```bash
./test                                        # Full suite: Rust tests + dev build + pytest
uv run pytest tests -v                        # Python tests only
uv run pytest tests/test_foo.py -v            # Single test file
uv run pytest tests/test_foo.py::TestClass::test_method -v  # Single test
RUNNING_PROCESS_LIVE_TESTS=1 uv run pytest -m live tests -v  # Integration tests
```

**Linting:**
```bash
./lint                           # Full suite: ruff + black + isort + pyright + KBI checker
uv run ruff check --fix src tests
uv run black src tests
uv run pyright src tests
```

**Environment:**
```bash
. ./activate.sh              # Activate dev environment (git-bash on Windows)
./install                    # Bootstrap Rust toolchain (rustup + pinned version)
```

## Daemon

```bash
running-process-daemon start|stop|status|list|kill-zombies
```

**Environment variables:**
- `RUNNING_PROCESS_NO_TRACKING=1` — disable daemon IPC
- `RUNNING_PROCESS_DAEMON_SCOPE=dev` — CWD-scoped daemon for test isolation
- `RUST_LOG=debug` — daemon log level

## CLIs

Two entry points in `pyproject.toml`:
- `running-process` → `running_process.cli:main` (daemon control, process listing)
- `running-processor` → `running_process.processor_cli:main` (dashboard web UI)

## Agent Backlog

Active pending work lives in [docs/AGENT_TASKS.md](docs/AGENT_TASKS.md). Root-level scratch task files are historical breadcrumbs.

## Windows Native Build Rules

- The canonical local rebuild path is `uv run build.py` — do not use raw `cargo build`
- `uv run build.py --dev` and `uv run build.py --quick` are the same mode
- Prefer repo entrypoints (`./install`, `./test`, `./_cargo`) over ad hoc cargo commands
- When a native dependency needs a C compiler, run from a Visual Studio developer shell or through `VsDevCmd.bat`
- Force the build target to `x86_64-pc-windows-msvc` when the environment is ambiguous; otherwise crates like `libsqlite3-sys` may try the GNU toolchain and fail looking for `gcc.exe`
- If a rebuild behaves like a GNU build on Windows, check the active shell environment before changing Rust code

## Code Conventions

**Imports**: Use fully qualified absolute imports (`from running_process.module import Class`, not relative `from .module import Class`)

**Subprocess commands**: Use `subprocess.list2cmdline()` instead of `str.join()` for proper shell escaping

**Output buffering**: `PYTHONUNBUFFERED=1` is automatically set for all spawned processes in `_create_process_with_pipe()` and `_create_process_with_pty()`

**Testing**: Use `unittest` framework (TestCase, assertEqual, etc.). Pytest is only the runner — avoid pytest-specific fixtures and decorators.

**Keyboard interrupts**: Use `handle_keyboard_interrupt(exception)` from `running_process.interrupt_handler` instead of directly calling `_thread.interrupt_main()`. The KBI linter (`ci/lint_python/keyboard_interrupt_checker.py`) enforces this.

## Code Quality Notes

- **Complex Functions** (refactor if modifying): `ProcessOutputReader.run()` (C12), `RunningProcess.get_next_line()` (C16), `RunningProcess.wait()` (C20)
- **Print Statements**: Console output via print() is intentional for CLI functionality
- **Exception Handling**: Broad exception handling is acceptable for process cleanup/recovery scenarios
- **Cross-Platform**: Code must work on Windows (MSYS), macOS, and Linux

## Workspace Config

- Rust edition 2021, version 1.85+, shared workspace dependencies: `pyo3 0.23`, `rusqlite 0.32` (bundled), `thiserror 2`
- Python requires >= 3.10, uses ABI3 stable API (`abi3-py310`)
- Release profile: line-tables-only debug info, packed split-debuginfo, no stripping
