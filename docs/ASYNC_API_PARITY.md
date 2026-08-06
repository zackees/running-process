# Async API parity

This is the checked-in parity inventory for #850. It records the public
operations currently implemented by the native async engine and the Python
bridge. A row marked **planned** is an explicit migration item, not an
implicit compatibility promise; it must be changed to **implemented** or
**N/A** with a rationale before #850 can close.

## Rust process surfaces

| Existing `NativeProcess` operation | Async equivalent | Status |
| --- | --- | --- |
| start / pid / wait | `AsyncProcess::start` / `pid` / `wait` | implemented |
| wait with timeout | `AsyncProcess::wait_timeout` | implemented |
| kill / stdin write / stdin close | `AsyncProcess::kill` / `write_stdin` / `close_stdin` | implemented |
| output / bounded output | `AsyncProcess::output` / `output_bounded` | implemented |
| one-shot run | `AsyncProcess::run` / `run_bounded` | implemented |
| timeout one-shot operations | `AsyncProcess::output_timeout` / `run_timeout` | implemented |
| output cursor and async cursor reads | `AsyncProcess::output_cursor` and `OutputCursor::read_next_async` | implemented |
| process-tree/group operations | actor containment command surface | planned: parity contract and public facade |

## Rust PTY surfaces

| Existing `NativePtyProcess` operation | Async equivalent | Status |
| --- | --- | --- |
| construction / start | `AsyncPtyProcess::new` / `start` | implemented |
| read with timeout | `AsyncPtyProcess::read_chunk` | implemented |
| write / resize | `AsyncPtyProcess::write` / `resize` | implemented |
| wait / pid | `AsyncPtyProcess::wait` / `pid` | implemented |
| terminate / kill / close | `AsyncPtyProcess::terminate` / `kill` / `close` | implemented |
| expect, idle detection, signal relay | async PTY semantic facade | planned |

## Python surfaces

| Existing sync surface | Async public surface | Status |
| --- | --- | --- |
| `RunningProcess` pipe lifecycle, stdin, output, timeout | `AsyncRunningProcess` | implemented for actor lifecycle/output; streaming parity planned |
| `PseudoTerminalProcess` lifecycle, read/write/resize | `AsyncPseudoTerminalProcess` | implemented for native PTY lifecycle; expect/idle parity planned |
| `InteractiveProcess` dispatch | `AsyncInteractiveProcess` | implemented for pipe and PTY dispatch |
| module-level process helpers | `running_process.asyncio` helpers | planned |
| sync GIL-release waits | native blocking adapters | planned: migration and heartbeat proof |

## Compatibility rules

- Existing sync names and import paths remain supported.
- Async rows must resolve to native Rust futures or actor commands; Python
  executors, `asyncio.to_thread`, polling, and reader threads are forbidden.
- Every **planned** row requires a focused RED -> GREEN test and an issue
  acceptance comment before it can be reclassified.
