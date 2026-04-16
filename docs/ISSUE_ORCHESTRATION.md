# Issue Orchestration Draft

This file is the draft body for a single orchestration issue that links the current open backlog.

I could not create the GitHub issue directly from the current tool permissions, so this document is the canonical draft.

## Summary

Track the current open work under one orchestration issue so the repo has:

- a clear dependency order
- one place to report status
- explicit sequencing between daemon work, CI hardening, and user-facing features

## Priority Order

### 1. Test and CI hardening

These items unblock future daemon and PTY work by making failures deterministic and diagnosable.

- #82 Standardize timeout crash and thread-dump watchdogs across unit and integration tests
- #46 fix(ci): supervised command 10s timeout kills cargo on cold CI caches
- #84 Review shared zccache cache strategy for PR builds
- #57 test: increase daemon crate code coverage to 90%

### 2. Daemon platform foundation

These define the architecture and delivery sequence for daemon support.

- #42 daemon processes
- #49 feat: first-class cross-platform daemon spawning
- #52 task: daemon spawning implementation phases
- #51 feat: GC cleanup for daemon binary trampolines

### 3. PTY robustness and stress coverage

These strengthen the runtime after the test harness is reliable.

- #69 read_batch() -> we need this
- #33 Add fuzz/stress testing for PTY and subprocess paths

### 4. User-facing tooling

These are valuable, but they should sit on top of the hardened runtime and daemon base.

- #63 feat: add `running-process dashboard` command with web UI
- #39 cli leak finder

## Proposed Execution Sequence

1. Finish #82 so every supported test entrypoint runs under the watchdog contract.
2. Close any remaining timeout-policy gaps from #46.
3. Revisit PR cache correctness in #84 only after the test wrappers are stable.
4. Raise daemon crate coverage in #57 to make daemon changes safer to land.
5. Continue daemon foundation work under #42, #49, #52, and #51 in that order.
6. Land PTY batching and stress work under #69 and #33.
7. Finish higher-level UX work in #63 and #39.

## Current Status Notes

- `#82` is the current active target.
- Unit tests already go through `ci.test`.
- Integration tests still need to consistently use the same supported wrapper path instead of bypassing it with direct `pytest` invocations.
- The daemon issues remain the largest block of architectural work.

## Linked Issues

- #82
- #46
- #84
- #57
- #42
- #49
- #52
- #51
- #69
- #33
- #63
- #39
