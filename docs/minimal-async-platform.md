# Minimal async platform substrate

Issue #1146 establishes the Phase 0.5 feature boundary used by kernal-api.
The supported selection is:

```toml
running-process = { version = "4.10.6", default-features = false, features = ["async-process"] }
```

It composes exactly one canonical Tokio actor implementation through
`running-process-platform-internal`. Tokio remains an implementation detail:
public APIs expose process contracts and futures, never Tokio types. There is
no synchronous fallback engine.

## Capability ownership

| Capability | Owner feature | Why it is present |
|---|---|---|
| Tokio process actor | `async-process` | Lifecycle, bounded output, cancellation, owner-death behavior. |
| Semantic process tree | `process-inspection` | The current portable `kill_tree` implementation is sysinfo-backed. This is a temporary Phase 0.5 composition, not a promise that all future inspection APIs are in the async surface. |
| PTY / ConPTY | `pty` | Terminal-specific process transport; it also composes semantic tree handling for PTY termination. |
| Local IPC identity | `ipc` | Owns interprocess, directories, and BLAKE3 endpoint hashing. |
| Async IPC / relay | `ipc-async` / `session-relay` | Composes IPC and the canonical Tokio capability. |
| ConPTY sidecar acquisition | `conpty-sidecar` | Windows-only download, archive, and SHA validation; never selected by async-process alone. |

`client`, the default feature set, and `daemon` retain process-inspection
forwarding for 4.x source compatibility. `running-process` itself explicitly
selects only the internal `process-inspection` primitive even with
`default-features = false`: `running_process::process_tree::kill_tree` is an
established containment API and its current implementation requires sysinfo.
That is the intentional Phase 0.5 containment exception. Direct public
inspection consumers remain behind `process-inspection`; the root does not
inherit the internal crate's compatibility defaults or any unrelated feature.

## ConPTY release hashes

The platform package has no build script and no build dependencies. Its hash
table is checked in at
`crates/running-process-platform-internal/src/platform_win/terminal/conpty_passthrough/conpty_sidecar_hashes.rs`.
For a release, `conpty-sidecar.sha256.toml` remains the release artifact input
and `ci/render_conpty_sidecar_hashes.py` renders it into that table before
wheel, binary, and crate builds. Normal builds therefore do not parse TOML or
compile ConPTY acquisition inputs merely because the platform crate appears in
the graph.

`ci.minimal_async_platform_graph` is the structural ratchet for this contract.
Feature-matrix validation additionally checks the resolver and behavior for
async-process, PTY/ConPTY, IPC/client, and daemon selections.
