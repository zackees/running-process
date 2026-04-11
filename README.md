# running-process

[![PyPI](https://img.shields.io/pypi/v/running-process)](https://pypi.org/project/running-process/) [![Crates.io](https://img.shields.io/crates/v/running-process-core)](https://crates.io/crates/running-process-core) [![codecov](https://codecov.io/gh/zackees/running-process/graph/badge.svg)](https://codecov.io/gh/zackees/running-process)

| Platform | Build | Lint | Unit Test | Integration Test |
|----------|-------|------|-----------|------------------|
| Linux x86 | [![Build](https://github.com/zackees/running-process/actions/workflows/linux-x86-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-x86-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/linux-x86-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-x86-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/linux-x86-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-x86-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/linux-x86-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-x86-integration-test.yml) |
| Linux ARM | [![Build](https://github.com/zackees/running-process/actions/workflows/linux-arm-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-arm-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/linux-arm-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-arm-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/linux-arm-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-arm-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/linux-arm-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/linux-arm-integration-test.yml) |
| Windows x86 | [![Build](https://github.com/zackees/running-process/actions/workflows/windows-x86-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-x86-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/windows-x86-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-x86-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/windows-x86-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-x86-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/windows-x86-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-x86-integration-test.yml) |
| Windows ARM | [![Build](https://github.com/zackees/running-process/actions/workflows/windows-arm-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-arm-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/windows-arm-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-arm-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/windows-arm-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-arm-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/windows-arm-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/windows-arm-integration-test.yml) |
| macOS x86 | [![Build](https://github.com/zackees/running-process/actions/workflows/macos-x86-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-x86-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/macos-x86-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-x86-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/macos-x86-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-x86-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/macos-x86-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-x86-integration-test.yml) |
| macOS ARM | [![Build](https://github.com/zackees/running-process/actions/workflows/macos-arm-build.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-arm-build.yml) | [![Lint](https://github.com/zackees/running-process/actions/workflows/macos-arm-lint.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-arm-lint.yml) | [![Unit Test](https://github.com/zackees/running-process/actions/workflows/macos-arm-unit-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-arm-unit-test.yml) | [![Integration Test](https://github.com/zackees/running-process/actions/workflows/macos-arm-integration-test.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/macos-arm-integration-test.yml) |

`running-process` is a Rust-backed subprocess runtime with a thin Python API.

## Why?

Subprocess on python sucks. I'm not going to get into this but if you know, you know. It's all fixable but you have to drop into native platform. running-process does exactly this and fixes all the problems.

This project started off as a fix for python's sub process module. It was in python originally, but then moved to OS specific rust. Now it's blazing fast. It also has cross platform process tracking, pty generation. It has zombie process tracking. It also has builtin `expect` for keyword event triggers, `idle tracking` (great for agent CLI's that dont' notifiy when they are done, they just stop sending data).

This libary is design for speed and correctness and portability. We get all three via near total code coverage.

## PTY Support Matrix

PTY support is a guaranteed part of the package contract on:

- Windows
- Linux
- macOS

On those platforms, `RunningProcess.pseudo_terminal(...)`, `wait_for_expect(...)`, and `wait_for_idle(...)` are core functionality rather than optional extras.

`Pty.is_available()` remains as a compatibility shim and only reports `False` on unsupported platforms.

## Pipe-backed API

```python
from running_process import RunningProcess

process = RunningProcess(
    ["python", "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
)

process.wait()

print(process.stdout)          # stdout only
print(process.stderr)          # stderr only
print(process.combined_output) # combined compatibility view
```

Captured data values stay plain `str | bytes`. Live stream handles are exposed separately:

```python
if process.stdout_stream.available():
    print(process.stdout_stream.drain())
```

Process priority is a first-class launch option:

```python
from running_process import CpuPriority, RunningProcess

process = RunningProcess(
    ["python", "-c", "import time; time.sleep(1)"],
    nice=CpuPriority.LOW,
)
```

`nice=` behavior:

- accepts either a raw `int` niceness or a platform-neutral `CpuPriority`
- on Unix, it maps directly to process niceness
- on Windows, positive values map to below-normal or idle priority classes and negative values map to above-normal or high priority classes
- `0` leaves the default scheduler priority unchanged
- positive values are the portable default; negative values may require elevated privileges
- the enum intentionally stops at `HIGH`; there is no realtime tier

Available helpers:

- `get_next_stdout_line(timeout)`
- `get_next_stderr_line(timeout)`
- `get_next_line(timeout)` for combined compatibility reads
- `stream_iter(timeout)` or `for stdout, stderr, exit_code in process`
- `drain_stdout()`
- `drain_stderr()`
- `drain_combined()`
- `stdout_stream.available()`
- `stderr_stream.available()`
- `combined_stream.available()`

`stream_iter(...)` yields tuple-like `ProcessOutputEvent(stdout, stderr, exit_code)` records.
Only one stream payload is populated per nonterminal item. When both pipes are drained, it yields
`(EOS, EOS, exit_code)` if the child has already exited, or `(EOS, EOS, None)` followed by a final
`(EOS, EOS, exit_code)` if the child closed both pipes before it exited.

`RunningProcess.run(...)` supports common `subprocess.run(...)` style cases including:

- `capture_output=True`
- `text=True`
- `encoding=...`
- `errors=...`
- `shell=True`
- `env=...`
- `nice=...`
- `stdin=subprocess.DEVNULL`
- `input=...` in text or bytes form

Unsupported `subprocess.run(...)` kwargs now fail loudly instead of being silently ignored.

## Expect API

`expect(...)` is available on both the pipe-backed and PTY-backed process APIs.

```python
import re
import subprocess
from running_process import RunningProcess

process = RunningProcess(
    ["python", "-c", "print('prompt>'); import sys; print('echo:' + sys.stdin.readline().strip())"],
    stdin=subprocess.PIPE,
)

process.expect("prompt>", timeout=5, action="hello\n")
match = process.expect(re.compile(r"echo:(.+)"), timeout=5)
print(match.groups)
```

Supported `action=` forms:

- `str` or `bytes`: write to stdin
- `"interrupt"`: send Ctrl-C style interrupt when supported
- `"terminate"`
- `"kill"`

Pipe-backed `expect(...)` matches line-delimited output. If the child writes prompts without trailing newlines, use the PTY API instead.

## PTY API

Use `RunningProcess.pseudo_terminal(...)` for interactive terminal sessions. It is chunk-oriented by design and preserves carriage returns and terminal control flow instead of normalizing it away.

```python
from running_process import ExpectRule, RunningProcess

pty = RunningProcess.pseudo_terminal(
    ["python", "-c", "import sys; sys.stdout.write('name?'); sys.stdout.flush(); print('hello ' + sys.stdin.readline().strip())"],
    text=True,
    expect=[ExpectRule("name?", "world\n")],
    expect_timeout=5,
)

print(pty.output)
```

PTY behavior:

- accepts `str` and `list[str]` commands
- auto-splits simple string commands into argv when shell syntax is not present
- uses shell mode automatically when shell metacharacters are present
- is guaranteed on supported Windows, Linux, and macOS builds
- keeps output chunk-buffered by default
- preserves `\r` for redraw-style terminal output
- supports `write(...)`, `read(...)`, `drain()`, `available()`, `expect(...)`, `resize(...)`, and `send_interrupt()`
- supports `nice=...` at launch
- supports `interrupt_and_wait(...)` for staged interrupt escalation
- supports `wait_for_idle(...)` with activity filtering
- exposes `exit_reason`, `interrupt_count`, `interrupted_by_caller`, and `exit_status`

`wait_for_idle(...)` has two modes:

- default fast path: built-in PTY activity rules and optional process metrics
- slow path: `IdleDetection(idle_reached=...)`, where your Python callback receives an `IdleInfoDiff` delta and returns `IdleDecision.DEFAULT`, `IdleDecision.ACTIVE`, `IdleDecision.BEGIN_IDLE`, or `IdleDecision.IS_IDLE`

There is also a compatibility alias: `RunningProcess.psuedo_terminal(...)`.

You can also inspect the intended interactive launch semantics without launching a child:

```python
from running_process import RunningProcess

spec = RunningProcess.interactive_launch_spec("console_isolated")
print(spec.ctrl_c_owner)
print(spec.creationflags)
```

Supported launch specs:

- `pseudo_terminal`
- `console_shared`
- `console_isolated`

For an actual launch, use `RunningProcess.interactive(...)`:

```python
process = RunningProcess.interactive(
    ["python", "-c", "print('hello from interactive mode')"],
    mode="console_shared",
    nice=5,
)
process.wait()
```

## Abnormal Exits

By default, nonzero exits stay subprocess-like: you get a return code and can inspect `exit_status`.

```python
process = RunningProcess(["python", "-c", "import sys; sys.exit(3)"])
process.wait()
print(process.exit_status)
```

If you want abnormal exits to raise, opt in:

```python
from running_process import ProcessAbnormalExit, RunningProcess

try:
    RunningProcess.run(
        ["python", "-c", "import sys; sys.exit(3)"],
        capture_output=True,
        raise_on_abnormal_exit=True,
    )
except ProcessAbnormalExit as exc:
    print(exc.status.summary)
```

Notes:

- keyboard interrupts still raise `KeyboardInterrupt`
- `kill -9` / `SIGKILL` is classified as an abnormal signal exit
- possible OOM conditions are exposed as a hint on `exit_status.possible_oom`
- OOM cannot be identified perfectly across platforms from exit status alone, so it is best-effort rather than guaranteed

## Text and bytes

Pipe mode is byte-safe internally:

- invalid UTF-8 does not break capture
- text mode decodes with UTF-8 and `errors="replace"` by default
- binary mode returns bytes unchanged
- `\r\n` is normalized as a line break in pipe mode
- bare `\r` is preserved

PTY mode is intentionally more conservative:

- output is handled as chunks, not lines
- redraw-oriented `\r` is preserved
- no automatic terminal-output normalization is applied

## Development

```bash
./install
./lint
./test
```

`./install` bootstraps `rustup` into the shared user locations (`~/.cargo` and `~/.rustup`, or `CARGO_HOME` / `RUSTUP_HOME` if you override them), then installs the exact toolchain pinned in `rust-toolchain.toml`. Toolchain installs are serialized with a lock so concurrent repo bootstraps do not race the same shared version.

`./lint` applies `cargo fmt` and Ruff autofixes before running the remaining lint checks, so fixable issues are rewritten in place.

`./test` runs the Rust tests, rebuilds the native extension with the unoptimized `dev` profile, runs the non-live Python tests, and then runs the `@pytest.mark.live` coverage that exercises real OS process and signal behavior.

On local developer machines, `./test` also runs the Linux Docker preflight so Windows and macOS development catches Linux wheel, lint, and non-live pytest regressions before push. GitHub-hosted Actions skip that Docker-only preflight and run the native platform suite directly.

If you want to invoke pytest directly, set `RUNNING_PROCESS_LIVE_TESTS=1` and run `uv run pytest -m live`.

For direct Rust commands, prefer the repo trampolines, which prepend the shared `rustup` proxy location:

```bash
./_cargo check --workspace
./_cargo fmt --all --check
./_cargo clippy --workspace --all-targets -- -D warnings
```

On Windows, native rebuilds that compile bundled C code should run from a Visual Studio developer shell. When the environment is ambiguous, point `maturin` at the MSVC toolchain binaries directly rather than relying on the generic cargo proxy.

For local extension rebuilds, prefer:

```bash
uv run build.py
```

That defaults to building a dev-profile wheel and reinstalling it into the repo's `uv` environment, which keeps the native extension in `site-packages` instead of copying it into `src/`. For publish-grade artifacts, use:

```bash
uv run build.py --release
```

## Process Containment

`ContainedProcessGroup` ensures all child processes are killed when the group is dropped, using OS-level mechanisms (Job Objects on Windows, process groups + `SIGKILL` on Unix).

```python
from running_process import ContainedProcessGroup

with ContainedProcessGroup() as group:
    proc = group.spawn(["sleep", "3600"])
# all children killed on exit, even on crash
```

### Crash-resilient orphan discovery

When a parent crashes, its in-process registry is lost. `ContainedProcessGroup` can stamp every child with an environment variable that survives parent death:

```python
from running_process import ContainedProcessGroup, find_processes_by_originator

# At launch: tag children with your tool name
with ContainedProcessGroup(originator="MYTOOL") as group:
    proc = group.spawn(["long-running-worker"])

# Later (from any process, any session): find orphans
stale = find_processes_by_originator("MYTOOL")
for info in stale:
    if not info.parent_alive:
        print(f"Orphaned PID {info.pid} from dead parent {info.parent_pid}")
```

The env var `RUNNING_PROCESS_ORIGINATOR=TOOL:PID` is inherited by all descendants. The scanner uses process start times to guard against PID reuse.

## Tracked PID Cleanup

`RunningProcess`, `InteractiveProcess`, and PTY-backed launches register their live PIDs in a SQLite database. The default location is:

- Windows: `%LOCALAPPDATA%\\running-process\\tracked-pids.sqlite3`
- Override: `RUNNING_PROCESS_PID_DB=/custom/path/tracked-pids.sqlite3`

If a bad run leaves child processes behind, terminate everything still tracked in the database:

```bash
python scripts/terminate_tracked_processes.py
```

## Notes

- `stdout` and `stderr` are no longer merged by default.
- `combined_output` exists for compatibility when you need the merged view.
- `RunningProcess(..., use_pty=True)` is no longer the preferred path; use `RunningProcess.pseudo_terminal(...)` for PTY sessions.
- On supported Windows builds, PTY support is provided by the native Rust extension rather than a Python `winpty` fallback.
- The test suite checks that `running_process.__version__`, package metadata, and manifest versions stay in sync.
