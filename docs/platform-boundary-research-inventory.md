# Phase-1 platform-boundary inventory protocol

This is the reviewed input to #968, not the transitional exact-occurrence ledger. It records the three-host collection method and current source distribution so phase 2 can reconcile a parser-derived union before freezing any baseline.

## Scope

Scan handwritten Rust under `crates/*/src`, `crates/*/tests`, `crates/*/examples`, `crates/*/benches`, and `testbins/src`; include crate roots, `build.rs`, inline tests, and test fixtures. Do not scan generated output, vendored sources, `target`, or Dylint UI fixtures. A candidate is assigned a capability (`process`, `terminal`, `ipc`, `fs`, `executable`, `host`, or `specialized`) and one of `attr_cfg`, `cfg_macro`, `compile_host_fact`, `native_import`, `native_path`, `concrete_module_ref`, or `legacy_platform_module`.

## Reconciliation protocol

Run the phase-2 parser/scanner with the same checkout on Windows, Linux, and macOS. Each run writes its own deterministic file, for example `platform-inventory/windows.tsv`; no process appends to a shared file. Merge by normalized occurrence key and preserve the union. A source walker is required in addition to compiler hooks, so wrong-host and orphaned module files remain visible. Commit the three raw files, union, and totals by crate/kind/capability with the phase-2 ledger.

## Phase-1 source audit summary

The broad source audit found host-bound code in every product package. `running-process` is the dominant owner (spawn/containment, PTY, broker endpoints, runtime files, and host facts); `running-process-platform-internal` already owns the async process seam but currently contains all three host implementations in one file. `running-process-probe` owns snapshot/crash/injection mechanics; the daemon owns local endpoint and spool mechanics. The three interposers and GNU bridge are specialized artifacts; `test-watchdog` is a specialized test-tool candidate; `testbins` are test fixtures and remain in scanner scope.

The phase-1 broad text sweep reported 2,703 candidate hits. It is intentionally not a baseline: it includes comments and strings and cannot distinguish aliases or macro token trees. It proves the work is workspace-wide and must be replaced by phase 2's AST/token inventory. The count must never grandfather code.

## Manifest audit

Target-specific dependency tables currently appear in the platform-internal package, all three interposers, the Windows GNU bridge, and `test-watchdog`; other native dependencies occur in product manifests. Phase 2's manifest checker must permit them only in the evolved platform package and named specialized zones, then report every retained dependency alongside its source-zone contract.
