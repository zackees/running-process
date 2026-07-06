# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture Overview

A Rust-backed Python library (v4.5.7) for subprocess and PTY process management across Windows, macOS, and Linux.

### Layered Design

**Python layer** (`src/running_process/`) provides high-level APIs:
- **`RunningProcess`**: Pipe-backed subprocess wrapper with output streaming, process tree management, and timeout handling
- **`PseudoTerminalProcess`**: PTY-backed process wrapper with expect patterns, idle detection, and terminal input relay
- **`InteractiveProcess`**: Unified facade dispatching to either pipe or PTY backends
- **`ProcessOutputReader`**: Threaded reader draining stdout/stderr to prevent blocking
- **`RunningProcessManager`**: Thread-safe singleton registry for tracking active processes

**Rust workspace** (`crates/`):
- **`running-process`** (`crates/running-process/`): the only published Rust crate. Feature-gated subsystems:
  - **`core`** (always on) — OS-level subprocess abstraction (`NativeProcess` — pipe I/O, signaling, Job Objects/process groups, PTY via `portable-pty`).
  - **`client`** (default) — proto types (`src/proto/`) + sync IPC client (`src/client/`). Adds prost, interprocess, dirs.
  - **`daemon`** — full daemon runtime (`src/daemon/`). Adds tokio, rusqlite, tracing, etc.
  - Binaries in `src/bin/`: `runpm` (requires `client`), `daemon` (requires `daemon`), `trampoline` (no required-features), `running-process-broker-v1` / `running-process-broker-v2` (broker scaffold for #483/#488/#532), `running-process-cleanup`.
  - `proto/daemon.proto` compiled by `build.rs` (prost-build + protox).
- **`running-process-py`**: PyO3 bindings. Contains `NativePtyProcess` alongside the pipe backend. Exposes a unified `PyNativeProcess` with `NativeProcessBackend` enum dispatching to either `NativeRunningProcess` or `NativePtyProcess`. Depends on `running-process` with `features = ["client", "originator-scan"]`.
- **`running-process-observer`** (`crates/running-process-observer/`, publish=false): sidecar / file-hook tier for #539 follow-up #551. Behind the off-by-default `embed-helper` feature flag (`dep:dirs`, `dep:blake3`, and on Windows `dep:windows-sys`). Exposes `HookConfig`, `negotiate_hook_support()`, embed-and-extract cache (`helper_cache_dir`, `extract_helper_blob_to`), and the per-OS injection vehicles `inject_into_pid` (Windows) / `inject_via_env` (Linux + macOS). **Sidecar contract**: this is the ONLY place injection symbols may live — main `running-process` crate stays free of `CreateRemoteThread` / `dlopen` of interposers (enforced for AV / EDR static analysis).
- **`running-process-observer-interposer-{linux,macos,windows}`** (publish=false): per-OS cdylib + rlib interposers that ship the actual file-API detours (`open`/`openat`/`close`/`write`/`unlink`/`rename` and Windows equivalents — `CreateFileW`/`WriteFile`/`CloseHandle`/`DeleteFileW`/`MoveFileExW`). Linux uses `LD_PRELOAD` + `dlsym(RTLD_NEXT, …)`; macOS uses `DYLD_INSERT_LIBRARIES` (SIP / hardened-runtime carve-outs apply); Windows uses `retour::RawDetour` inline trampolines, gated on `x86_64` only (`retour 0.4.0-alpha.4` uses iced-x86 which doesn't support ARM64). Each emits `RPO_HOOK …` lines on stderr in a shared format. Non-target hosts compile to an inert rlib stub so the workspace builds end-to-end.
- **`running-process-win-gnu-bridge`** (`crates/running-process-win-gnu-bridge/`, publish=false): build seam (#580) exposing the MSVC-obligatory Windows API surface to `x86_64-pc-windows-gnu` builds. Inert no-op on MSVC / non-Windows; on `-gnu` it statically imports the ConPTY entry points (`CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`) directly from `windows-sys` (which bundles a per-target `-gnu` import lib), proving the surface links with no Windows SDK / MSVC `link.exe`. `retour` detours / DLL injection are validated under GNU; bundled `libsqlite3-sys` is the remaining follow-up. See `docs/win-gnu-bridge.md`.
- **`test-watchdog`** (`crates/test-watchdog/`, publish=false): cross-platform hang-dump helper used as dev-dep by `running-process` tests (procdump minidump on Windows, gdb/lldb all-thread backtraces on Unix).
- **`testbins`**: test-fixture binaries (`cwd-reporter`, `dies-after-spawn`, `emitter`, `env-dump`, `env-reporter`, `sleeper`, `slow-stdin-reader`, `spawner`, `stdin-echoer`, `stubborn`, `tui-counter`, `createfilew-probe`).

**Python-Rust bridge**: `running_process._native` module compiled via maturin. Python's `PseudoTerminalProcess.start()` calls `NativeProcess.for_pty()` which creates a `NativePtyProcess` on the Rust side.

## Development Commands

**Build (native extension):**
```bash
uv run build.py              # Dev wheel, reinstalls into venv (default)
uv run build.py --release    # Publish-grade wheels in dist/
```

**Testing:**
```bash
./test                                                  # Full suite: Rust tests + dev build + pytest
uv run --no-sync pytest tests -v                        # Python tests only (preserves the existing venv)
uv run --no-sync pytest tests/test_foo.py -v            # Single test file
uv run --no-sync pytest tests/test_foo.py::TestClass::test_method -v  # Single test
RUNNING_PROCESS_LIVE_TESTS=1 uv run --no-sync pytest -m live tests -v  # Integration tests
```

**`uv run` policy.** Bare `uv run …` is **blocked by the pre-tool hook** because it auto-syncs the maturin project and forces a full native rebuild on every invocation (see zackees/soldr#805). Always pass `--no-project` for pure-Python scripts, `--no-sync` to reuse the warm venv, or `--frozen` to lock to the existing lockfile. The escape hatch for a legitimate full-rebuild is `./test`.

**Per-test deadlock guard.** Every test (Rust + Python) gets a hard 2-minute wall-clock kill so a hung test can't stall CI indefinitely:
- Rust runs through `cargo nextest` (auto-installed by `ci/test.py` if missing); `.config/nextest.toml` sets `slow-timeout.terminate-after = 2 × 60s`. On fire nextest prints `TIMEOUT [...] <crate>::<test_file> <test_name>` plus captured stdout/stderr.
- Python uses `pytest-timeout` with `timeout = 120, timeout_method = "thread"` in `pyproject.toml`. On fire pytest prints a `+++ Timeout +++` banner with every thread's Python stack — enough to identify the hung test from CI logs.
- Rust tests that opt into `test_watchdog::install(timeout, message, dump_path)` (e.g. `tests/containment_test.rs`) additionally get an out-of-process dump *before* nextest's kill: on Windows a full minidump via `procdump -ma`; on Linux/macOS all-thread backtraces via `gdb -p <pid> -batch -ex 'thread apply all bt'` (or `lldb --batch -o 'thread backtrace all'`), printed to stderr and written to `dump_path`. Works for non-cooperative hangs (thread blocked in a syscall); on Linux the watchdog sets `PR_SET_PTRACER_ANY` so the child debugger may attach even under Yama `ptrace_scope=1`. Missing debugger → one-line note, never an extra failure.
Override per-invocation when needed: `cargo nextest run -- --slow-timeout 30s --terminate-after 1` or `pytest --timeout=300`.

**Linting:**
```bash
./lint                           # Full suite: ruff + black + isort + pyright + KBI checker + spawn-path-guard
uv run ruff check --fix src tests
uv run black src tests
uv run pyright src tests
```

The lint pass also runs `ci/spawn_path_guard.py`, which forbids raw `Command::new` / `.spawn()` / `portable_pty` / `CreatePipe` / `ChildStd*::from` outside the sanitized spawn layer. New call sites need an explicit allowlist entry with a justification comment — see existing entries for the shape.

**Wrong toolchain?** Invoke build commands as `soldr cargo …`, `soldr rustc …`, `soldr rustfmt …`. The globally installed [soldr](https://github.com/zackees/soldr) binary resolves the rustup-managed toolchain via `rustup which` — handy on Windows where chocolatey cargo or other stale shims can take precedence on PATH. Install soldr globally (it is no longer pulled in as a uv dev dep) — e.g. `pipx install soldr` or `cargo install soldr`. CI Python (`ci/soldr.py:cargo_command`) detects soldr on PATH and routes through it automatically, falling back to raw `cargo` on CI runners where soldr isn't installed.

**Environment:**
```bash
. ./activate.sh              # Activate dev environment (git-bash on Windows)
./install                    # Bootstrap Rust toolchain; builders use soldr
```

Project hook policy: `.claude/settings.json` mandates that direct soldr-supported Bash build commands (`cargo build|check|test|package|publish`, `rustc`, `rustfmt`, `clippy-driver`) are prefixed with `soldr` (the globally installed binary). Raw commands are denied — use `soldr cargo ...` or one of the higher-level repo entrypoints (`uv run build.py`, `./install`, `./lint`, `./test`).

## Daemon

```bash
running-process-daemon start|stop|status|list|kill-zombies
```

**Environment variables:**
- `RUNNING_PROCESS_NO_TRACKING=1` — disable daemon IPC
- `RUNNING_PROCESS_DAEMON_SCOPE=dev` — CWD-scoped daemon for test isolation
- `RUST_LOG=debug` — daemon log level
- `RUNNING_PROCESS_FAKE_BACKEND=<path>` — TEST-ONLY broker seam: `connect_to_backend` dials `<path>` directly, skipping broker negotiation entirely (never set in production; `RUNNING_PROCESS_DISABLE=1` takes precedence)
- `RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED=1` — opt out of the broker-v2 "refuse privileged startup" guard (test-only; defaults to refusing root)

## Broker

`running-process-broker-v2` is the v2 transport (see #483 / #488 / #532). Bind path derivation lives in `src/broker/lifecycle/names_v2.rs` (`rpb-v2-{program}-{sid_hash}-{pipe_idx}`); the resolved socket path goes under `$XDG_RUNTIME_DIR/running-process/broker-v2/` on Linux, `$TMPDIR/.rp-<uid>-broker-v2/` on macOS (hashed leaf to fit `sun_path`), or `\\.\pipe\…` on Windows. `is_already_bound_error` classifies `AddrInUse | WouldBlock | PermissionDenied` as already-bound — `PermissionDenied` is included because Windows double-bind surfaces as `ERROR_ACCESS_DENIED` (raw os error 5) via the existing pipe instance's ACL.

## File-Hook Tier (#551)

Off-by-default opt-in via the `running-process-observer` crate's `embed-helper` feature. When enabled, `negotiate_hook_support()` returns `HookSupport::Available` on Windows + Linux + macOS. The injection vehicle is per-OS:

- **Windows**: `inject_into_pid(pid, dll_path)` drives `OpenProcess` → `VirtualAllocEx` → `WriteProcessMemory` → `CreateRemoteThread(LoadLibraryW, dll_path)` → `WaitForSingleObject` + `GetExitCodeThread`. The injected DLL's `DllMain` defers `retour::RawDetour` install to a `CreateThread` worker (retour's iced-x86 prologue analysis + `VirtualProtect` re-enter the loader lock; inline install hangs `LoadLibraryW`).
- **Linux + macOS**: `inject_via_env(command, interposer_path)` sets the platform's loader env var (`LD_PRELOAD` / `DYLD_INSERT_LIBRARIES`) on a caller-supplied `Command`. The dynamic linker handles the rest at child startup.

The interposers emit `RPO_HOOK …` lines on the target's stderr (e.g. `RPO_HOOK file-open path="…" access=0x… disposition=… handle=…`). All injection symbols live in the observer crate; the main `running-process` crate compiles with **zero** new injection-related symbols (verified end-to-end).

## CLIs

Two entry points in `pyproject.toml`:
- `running-process` → `running_process.cli:main` (daemon control, process listing)
- `running-processor` → `running_process.processor_cli:main` (dashboard web UI)

## Releasing

Releases are driven by the **Auto Release** workflow (`.github/workflows/auto-release.yml`).

Full operator guide — trigger conditions, one-time prerequisites
(PyPI trusted publisher, `CARGO_REGISTRY_TOKEN`), the version-bump
checklist that `ci/version_check.py` enforces, what each job
publishes, and recovery for common failure modes — lives in
[docs/RELEASING.md](docs/RELEASING.md).

Quick local sanity check before cutting a release:
```
uv run --no-project --module ci.version_check
```
(`--no-project` skips the maturin auto-sync — `ci.version_check` only reads version strings out of `pyproject.toml`/`Cargo.toml`/`__init__.py` and doesn't need the native module.)

## Agent Backlog

Active pending work lives in [docs/AGENT_TASKS.md](docs/AGENT_TASKS.md). Root-level scratch task files are historical breadcrumbs.

## Windows Native Build Rules

- The canonical local rebuild path is `uv run build.py` — do not use raw `cargo build`
- `uv run build.py --dev` and `uv run build.py --quick` are the same mode
- Prefer repo entrypoints (`./install`, `./test`, `./lint`, `uv run build.py`) over ad hoc cargo commands
- When a native dependency needs a C compiler, run from a Visual Studio developer shell or through `VsDevCmd.bat`
- Force the build target to `x86_64-pc-windows-msvc` when the environment is ambiguous; otherwise crates like `libsqlite3-sys` may try the GNU toolchain and fail looking for `gcc.exe`
- If a rebuild behaves like a GNU build on Windows, check the active shell environment before changing Rust code

## Code Conventions

**Imports**: Use fully qualified absolute imports (`from running_process.module import Class`, not relative `from .module import Class`)

**Subprocess commands**: Use `subprocess.list2cmdline()` instead of `str.join()` for proper shell escaping

**Output buffering**: `PYTHONUNBUFFERED=1` is automatically set for all spawned processes in `_create_process_with_pipe()` and `_create_process_with_pty()`

**Testing**: Use `unittest` framework (TestCase, assertEqual, etc.). Pytest is only the runner — avoid pytest-specific fixtures and decorators.

**Keyboard interrupts**: Use `handle_keyboard_interrupt(exception)` from `running_process.interrupt_handler` instead of directly calling `_thread.interrupt_main()`. The KBI linter (`ci/lint_python/keyboard_interrupt_checker.py`) enforces this.

**Bincode forbidden**: `disallowed_methods = "deny"` is wired through `clippy.toml` at the workspace root — every member crate refuses bincode serialization (broker wire stays prost-only). Phase 0 of #228.

## Code Quality Notes

- **Complex Functions** (refactor if modifying): `ProcessOutputReader.run()` (C12), `RunningProcess.get_next_line()` (C16), `RunningProcess.wait()` (C20)
- **Print Statements**: Console output via print() is intentional for CLI functionality
- **Exception Handling**: Broad exception handling is acceptable for process cleanup/recovery scenarios
- **Cross-Platform**: Code must work on Windows (MSYS), macOS, and Linux

## Workspace Config

- Rust edition 2021, version 1.85+, shared workspace dependencies: `pyo3 0.29`, `rusqlite 0.32` (bundled), `thiserror 2`
- Python requires >= 3.10, uses ABI3 stable API (`abi3-py310`)
- Release profile: line-tables-only debug info, packed split-debuginfo, no stripping
