# Host-platform boundary: phase 1 research gate

This note records the phase-1 architecture decision for [#965](https://github.com/zackees/running-process/issues/965) and [#967](https://github.com/zackees/running-process/issues/967). It deliberately makes no production ownership change. Phase 2 must consume this note and its inventory before a mechanical `cfg` rewrite.

## Decision

Evolve the published `running-process-platform-internal` package in place. Consumers import it internally as `crate::platform`; the package name remains an implementation detail. A second package would require a lockstep published-package migration without making the ownership boundary clearer.

The package becomes a dependency leaf: it may depend on external implementation libraries, but not on another running-process workspace package, PyO3, daemon/probe protocol types, or broker policy. Its `lib.rs` will contain the only production host selector, using `std::cfg_select!`, with exactly `target_os = "windows"`, `target_os = "linux"`, and `target_os = "macos"` arms and no fallback. Linux and macOS keep separate concrete trees; there is no permanent `platform_unix`.

`platform::{process, terminal, ipc, fs, executable, host}` is the neutral facade. It exchanges standard-library values or facade-owned types only: never raw handles/file descriptors, Win32/libc structs or error codes, Tokio process values, concrete streams/pipes, concrete PTYs, Python/PyO3 types, or daemon/broker/probe protocol types. Callers retain endpoint naming policy, lifecycle/retry policy, protocol framing, public diagnostics, and release-target selection.

The sole transitional PTY API exception is source compatibility for the public
4.x `PtyMaster::{process_group_leader, as_raw_fd}` and
`PtyChild::as_raw_handle` method names, plus the deprecated
`pty::reexports::portable_pty` helpers. These accessors are never consumed by
platform mechanics: preparation, interrupts, process-group control, and PID
selection use facade operations whose native state stays inside the concrete
tree. The compatibility surface is deprecated for removal in 5.0; raw control
payload structs or tokens remain forbidden in the neutral facade.

The equivalent IPC transition preserves the public 4.x broker post-Hello
callback's `interprocess::local_socket::Stream` parameter until 5.0. The
internal package may provide a hidden crate-root conversion at that final API
boundary, but `platform::ipc` remains opaque and no product mechanics may
consume or expose the concrete transport.

## Toolchain and packaging proof

`std::cfg_select!` is stable since Rust 1.95.0 ([Rust source](https://doc.rust-lang.org/src/core/macros/mod.rs.html#231-235)); the current 1.94.1 toolchain and declared MSRV 1.85 cannot use it. Phase 2 raises the root toolchain pin and workspace `rust-version` to **1.95.0** (or a later reviewed patch release) atomically. The CI Dylint lane remains independently pinned to `nightly-2026-04-16` until its upgrade is separately validated.

The affected pins are the root toolchain file, workspace manifest, both development Dockerfiles, preflight Dylint tool install line, and user-facing references in `README.md`, `CLAUDE.md`, and reproducibility documentation. `install` and `ci/env.py` already read the root toolchain file. Phase 2 must classify fixture-only pins before changing them.

All four published packages (`running-process`, `running-process-py`, `running-process-probe`, and `running-process-probe-daemon`) inherit the lockstep workspace version checked by `ci/version_check.py`. Evolving the existing internal package preserves its identity and the `async-process` optional dependency surface of `running-process`; it avoids an extra published dependency and keeps synchronous consumers free of Tokio. Maturin continues to build the ABI3 Python extension from `running-process-py`; no Python API or wheel target change is implied.

## Inventory and migration order

The initial inventory is source-based, so it sees inactive code as well as the active host. The reproducible scan and host-reconciliation protocol are in `docs/platform-boundary-research-inventory.md`.

| Capability | Initial owners |
| --- | --- |
| `process` | spawn implementations, containment, process tree/observer, daemon launch/kill, trampoline |
| `terminal` | PTY, ConPTY passthrough, terminal input/graphics, console and window-icon mechanics |
| `ipc` | daemon/client endpoints, broker listener/connect/handoff, peer identity and endpoint security |
| `fs` / `executable` | runtime/spool/discovery ownership, safe opening, replacement/shadow copies and sibling images |
| `host` | directories, UID/SID/elevation, resource pressure and autostart |

The audit establishes `fs.runtime_dir -> ipc.security`: endpoint security needs owner-private directory primitives. The phase order is therefore **process, terminal, fs/executable, ipc, host, exceptions**. #971 must be rebased conceptually on #972 when phase 2 freezes the ledger.

## Exceptional-component decisions

| Component | Decision | Boundary contract |
| --- | --- | --- |
| Probe snapshot/unwinding | Retain named specialized zone | Signal-handler and suspend/resume code remains local: facade calls must not compromise async-signal safety or realtime-signal ownership. |
| Crash handler / crash spool | Split | Owner-private filesystem primitives migrate to `platform::fs`; callback mechanics remain a signal-safety zone. |
| `embed-helper` injection | Retain named zone | Injection symbols remain only in `running-process-probe`; `running-process` gains no remote-thread or interposer-loading symbols. |
| Linux/macOS/Windows interposers | Retain three named zones | Their cdylib identity and loader ABI remain separate; each permits only its loader/native APIs and viability guard. |
| Windows GNU bridge | Retain named zone | Preserve its Windows GNU import-library link proof and reject unrelated platform selection. |
| `test-watchdog` | Test-tool zone pending migration | It may retain debugger/minidump mechanics under an exact test-tool profile. |

These are profiles, not directory exemptions. Phase 2's scanner/Dylint must enforce their per-artifact contracts and reject cross-platform references.

## Host versus compiler target

The selector represents the machine running process, filesystem, and local IPC mechanics. It must not be inferred from `--target`: a Linux host producing a Windows artifact still uses Linux host filesystem/process/IPC behavior. The target controls only generated artifact ABI, including an interposer cdylib's target. Phase 9 records this with Linux-hosted cross-target validation.

## RED evidence

The current boundary lint is a `LateLintPass::check_expr` that inspects only snippets containing `Command`. It has no attribute, macro-token, import-alias, module-reference, or whole-tree hooks. Consequently private and inactive host code is accepted today. The fixture `lints/running-process-platform-boundary/ui/research_red_pass.rs` compiles without a boundary diagnostic despite a private host `cfg`, an inactive wrong-host native import, and a `cfg!` expression. Phase 2 turns this evidence into negative UI tests and a parser-based whole-tree scan.

## Phase-2 exit requirements

Before a capability migration, phase 2 must reconcile actual Windows, Linux, and macOS scanner inventories; freeze the exact-occurrence ledger; implement the one-selector skeleton; replace the placeholder Dylint with pre-expansion analysis; and add independent parser-based scanner and manifest checker.
