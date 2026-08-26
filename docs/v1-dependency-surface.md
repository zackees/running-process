# v1 Dependency Surface

This document records the current direct runtime dependency surface for the
`running-process` crate. It is security evidence for #241, but it is not the
final v1.0.0 security-review signoff.

The inventory below is machine-checked by
`crates/running-process/tests/security/dependency_surface.rs`. Any direct
runtime dependency added to `crates/running-process/Cargo.toml` must be listed
here in the same change.

## Direct Runtime Dependency Inventory

| Dependency | Manifest section | Activation | Review note |
|---|---|---|---|
| `libc` | `[dependencies]` | Always compiled | Unix process, file, and credential APIs. Platform boundary is security-sensitive. |
| `sysinfo` | `[dependencies]` | Always compiled | Process inspection for local runtime behavior. No network transport purpose. |
| `thiserror` | `[dependencies]` | Always compiled | Error types only. Workspace version source. |
| `winapi` | `[dependencies]` | Always compiled; used by Windows paths | Windows process, pipe, handle, and security APIs. Platform boundary is security-sensitive. |
| `prost` | `[dependencies]` | `client` feature | Protobuf decode/encode for v1 broker/control types. Untrusted-input parser. |
| `prost-types` | `[dependencies]` | `client` feature | Protobuf well-known types used with prost-generated structures. |
| `interprocess` | `[dependencies]` | `client` feature | Local IPC abstraction for named pipes and Unix-domain sockets. Security-sensitive IPC boundary. |
| `dirs` | `[dependencies]` | `client` feature | Current-user config/cache directory discovery. Filesystem trust-boundary input. |
| `anyhow` | `[dependencies]` | `client` feature | CLI/client error propagation only. |
| `clap` | `[dependencies]` | `client` feature | `runpm` CLI argument parsing. Operator input boundary. |
| `blake3` | `[dependencies]` | `client` feature | Per-user broker identity hashing. No network transport purpose. |
| `sha2` | `[dependencies]` | `client` feature | Manifest and binary digest verification. Security-sensitive integrity primitive. |
| `getrandom` | `[dependencies]` | `client` feature | Backend pipe randomness. Security-sensitive entropy boundary. |
| `running-process-probe` | `[dependencies]` | `probe` feature | Schema-only: supplies the `probe_diag.v1` prost types to the probe client facade (#633). Pulled in with `default-features = false`, so the crate's injection vehicles (gated on its `embed-helper` feature) are never compiled here and the main crate keeps zero injection symbols. No network transport purpose. |
| `running-process-protocol` | `[dependencies]` | `client` feature | Published implementation detail owning broker and daemon protobuf generation (#1144). Process-only consumers omit its prost build dependencies; callers retain the client-gated `running_process::proto` and broker protocol re-exports. |
| `tokio` | `[dependencies]` | `daemon` feature | Async runtime for broker daemon tasks. `full` features include broad Tokio APIs, so code review must keep broker operation local-IPC-only. |
| `running-process-platform-internal` | `[dependencies]` | Always compiled; optional capabilities follow `pty`, `conpty-sidecar`, and `client-async` | Blessed platform boundary for process, terminal/PTY/ConPTY, terminal input, and native window-icon mechanics. It owns the concrete host dependencies and exposes neutral caller-facing types. |
| `tokio-util` | `[dependencies]` | `daemon` feature | Codec helpers for local IPC framing. Untrusted-input framing boundary. |
| `bytes` | `[dependencies]` | `daemon` feature | Buffer type used by async framing. Untrusted-input sizing boundary. |
| `futures-util` | `[dependencies]` | `daemon` feature | Async sink/stream helpers. No standalone transport purpose. |
| `tracing` | `[dependencies]` | `daemon` feature | Local observability. Must not log secrets or trusted handle material. |
| `tracing-subscriber` | `[dependencies]` | `daemon` feature | Local logging subscriber. No network exporter enabled. |
| `rusqlite` | `[dependencies]` | `daemon` feature | Local SQLite state. Workspace version source with bundled SQLite. |
| `toml` | `[dependencies]` | `daemon` feature | Service-definition parsing. Untrusted-input parser. |
| `serde` | `[dependencies]` | Always compiled | Data model derives and local JSON sidecar support. Not broker wire authority. |
| `serde_json` | `[dependencies]` | Always compiled | Local JSON sidecar/admin-output support. The broker wire format remains prost-only. |
| `windows-sys` | `[target.'cfg(windows)'.dependencies]` | Windows only | ConPTY and Windows platform APIs. Platform boundary is security-sensitive. |

## Current Review Summary

- `running-process-platform-internal` is the only direct dependency whose
  transitive, Windows-only `conpty-sidecar` capability includes an HTTP/TLS
  client: `ureq`, gated to a single one-shot GET to this crate's own GitHub
  release for the Win10 ConPTY sidecar (see #445). It is reachable only from
  `platform_win::terminal::conpty_passthrough::conpty_acquire`, never from
  broker code.
  No other current direct runtime dependency is reviewed as an HTTP, TLS,
  WebSocket, browser-facing transport, or network-RPC dependency.
- `tokio` is the only direct runtime dependency with broadly available async
  network APIs through its enabled feature set. The v1 no-network commitment is
  enforced at the broker code and syscall-behavior level, not by pretending
  those APIs are absent from Tokio.
- The broker wire format remains prost-only; bincode is not present as a direct
  runtime dependency.
- `serde` and `serde_json` are present for local sidecar/admin-output paths.
  They are not authority for the broker wire format. Any plan to remove or
  replace them belongs in a follow-up dependency-minimization issue.
- Windows platform APIs are split between `winapi` and `windows-sys`. That is a
  migration state, not a request to add more platform API crates.

## Change Rules

Before merging a runtime dependency change:

- Run `cargo audit --deny warnings`.
- Update this inventory in the same PR.
- Re-check whether the dependency or enabled features add HTTP, TLS,
  WebSocket, browser-facing transport, network RPC, or new file/process
  authority.
- Reject dependencies added for trivial formatting, parsing, path handling, or
  command glue unless the design issue explains why local code is less safe.
- Treat broker parsing, IPC, manifest, service-definition, cleanup, handoff,
  lifecycle, and platform-boundary dependencies as security-sensitive.
