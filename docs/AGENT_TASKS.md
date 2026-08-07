# Agent Tasks

This file is the active backlog for agent-driven work. Root-level scratch task files should not carry open items.

## Active Backlog

### Runtime Tiny-PDB Validation

- Add a Windows-native debugger or symbolizer path that reliably consumes the shipped tiny PDB during live stack-dump tests.
- Make the strict path used by `bash ./test --no-skip` pass on supported Windows environments instead of failing or skipping because the local debugger cannot resolve PDB-backed frames.
- Keep static `llvm-pdbutil` verification as the baseline release gate until the runtime debugger path is trustworthy.

### Async Engine — post-#875 follow-ups (was meta #886)

The `#886` sequencing tracker is retired: its job was to order the work left
after `#875`, and the one PR-sized child has landed. The remaining two items
are not PR-sized and continue as **standalone** trackers rather than blocking a
meta:

- **Done** — kill-child-when-owner-dies spawn option (`#885`, merged in `#888`).
  Lives on the canonical async `SpawnSpec` seam: Linux `PR_SET_PDEATHSIG` with a
  fork/exec race guard, a concrete macOS `kqueue`/`EVFILT_PROC` supervisor, and a
  Windows `KILL_ON_JOB_CLOSE` Job Object. Exercised by a cross-process test that
  force-kills the owner (SIGKILL / `TerminateProcess`, skipping `Drop`).
- **Ongoing — `#850`** the architecture inversion (Tokio becomes the canonical
  engine; sync wraps async). An 8-phase acceptance program — native soak jobs on
  all three platforms, locked perf-regression budgets, fault-injection suites,
  wheel + packed-crate publish tests, zero-debt Dylint canaries, and a Phase-8
  acceptance report — governed by design authority `#849`. Wants a slice plan
  before code.
- **Ongoing — `#882`** the coverage-only flake in
  `kill_is_delivered_while_output_is_draining`. `#881` removed the pipe-EOF hang
  path (direct spawn, no pipe-holding grandchild) and it has been green since;
  the root-cause fix (ack `kill` on signal delivery vs exit observation) is a
  public-semantic change deferred to a `#849` decision.

### NativeProcess Migration

Design reference:
- [REFACTOR_NATIVE_PROCESS.md](C:/Users/niteris/dev/running-process/REFACTOR_NATIVE_PROCESS.md)

Remaining phases:
- Phase 4: move output buffering, history, and checkpoints into `NativeProcess`
- Phase 5: move `wait_for` orchestration into `NativeProcess`
- Phase 6: move `expect` lifecycle and EOF handling deeper into native code
- Phase 7: move idle detection into `NativeProcess`
- Phase 8: simplify Python `RunningProcess` into a thin facade

Execution notes:
- Rebuild with `./.venv/Scripts/python.exe build.py` before trusting Python test results.
- Run targeted PTY and subprocess tests after each phase.
- Keep phases small and coherent; do not start a new phase while the current one is unstable.

## Archived Root Task Files

The following root files are retained only as redirects or historical breadcrumbs:
- `TASK.md`
- `TODO.md`
- `PLAN_TINY_PDB.md`
