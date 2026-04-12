# Agent Tasks

This file is the active backlog for agent-driven work. Root-level scratch task files should not carry open items.

## Active Backlog

### Runtime Tiny-PDB Validation

- Add a Windows-native debugger or symbolizer path that reliably consumes the shipped tiny PDB during live stack-dump tests.
- Make the strict path used by `bash ./test --no-skip` pass on supported Windows environments instead of failing or skipping because the local debugger cannot resolve PDB-backed frames.
- Keep static `llvm-pdbutil` verification as the baseline release gate until the runtime debugger path is trustworthy.

### NativeProcess Migration

Design references:
- [RUST_PYTHON_BOUNDARY.md](docs/RUST_PYTHON_BOUNDARY.md) — cross-boundary patterns (atomic flags, Arc-shared cores, event queues)
- [REFACTOR_NATIVE_PROCESS.md](C:/Users/niteris/dev/running-process/REFACTOR_NATIVE_PROCESS.md)

Completed:
- Phase 7: idle detection moved into native Rust (PR #26)
  - `IdleDetectorCore` extracted as `Arc`-shareable struct
  - PTY reader thread feeds idle detector directly (no GIL)
  - `NativePtyProcess.wait_for_idle()` blocks entirely in Rust
  - Echo output handled natively via `Arc<AtomicBool>` flag
- Phase 7b: echo output moved into native Rust (same PR)
  - Reader thread writes to stdout when `echo` flag is set
  - No Python callback needed during idle wait

Remaining (tracked in issue #25):
- Phase 4: move output buffering, history, and checkpoints into `NativeProcess`
- Phase 5: move `wait_for` orchestration into `NativeProcess`
- Phase 6: move `expect` lifecycle and EOF handling deeper into native code
- Phase 8: simplify Python `RunningProcess` into a thin facade
- Simplify Python `wait_for_idle()` to call `proc.wait_for_idle(detector, timeout)` directly

Execution notes:
- Rebuild with `uv run --module ci.build_wheel --dev` before trusting Python test results.
- Run targeted PTY and subprocess tests after each phase.
- Keep phases small and coherent; do not start a new phase while the current one is unstable.
- Follow patterns in [RUST_PYTHON_BOUNDARY.md](docs/RUST_PYTHON_BOUNDARY.md) for all new cross-boundary work.

## Archived Root Task Files

The following root files are retained only as redirects or historical breadcrumbs:
- `TASK.md`
- `TODO.md`
- `PLAN_TINY_PDB.md`
