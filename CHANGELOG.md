# Changelog

## 4.0.0 — Mono-crate consolidation

**Breaking change for direct Rust consumers only.** The Python (`pip install running-process`) ABI is unchanged.

The `running-process-core`, `-proto`, `-client`, `-daemon`, and `daemon-trampoline` crates have been merged into a single **`running-process`** crate with feature-gated subsystems. Only `running-process-py` remains separate as the PyO3 binding (per the [language-bindings-only resolution in #165](https://github.com/zackees/running-process/issues/165)).

### Migration for direct Rust consumers

| Before | After |
|---|---|
| `running-process-core = "3.x"` | `running-process = "4.0.0"` (with `default-features = false` if you want only the spawn API) |
| `running-process-proto = "3.x"` | `running-process = "4.0.0", features = ["client"]` (proto types reachable at `running_process::proto::*`) |
| `running-process-client = "3.x"` | `running-process = "4.0.0", features = ["client"]` (default; reachable at `running_process::client::*`) |
| `running-process-daemon = "3.x"` | `running-process = "4.0.0", features = ["daemon"]` (reachable at `running_process::daemon::*`) |
| `cargo install runpm-cli` | `cargo install running-process` (the default `client` feature pulls `runpm` in) |
| `cargo install running-process-daemon` | `cargo install running-process --features daemon --bin running-process-daemon` |

### Feature scheme

- **`core`** (always available) — spawn API, PTY, containment.
- **`client`** (default) — proto types + sync IPC client to talk to a daemon. Adds prost, interprocess, dirs.
- **`daemon`** — full daemon runtime. Superset of `client`. Adds tokio, rusqlite, tracing, etc.
- **`originator-scan`** — used by `running-process-py` for cwd-tagging.

### Why

- **Publish surface:** 6 crates → 2 (running-process + running-process-py). Single version-bump motion at release time.
- **Dependency clarity:** consumers pick the subsystem they need with one feature flag instead of three path-deps. Tree-shaking via `required-features` on binaries means `cargo install` builds only what the chosen binary needs.
- **No more cross-crate plumbing:** ~120 `running_process_core::` / `running_process_proto::` / `running_process_client::` / `running_process_daemon::` imports collapsed to `crate::*` (lib) or `running_process::*` (binaries / tests).

### Python — unchanged

The PyPI `running-process` wheel ships with the same Python API. Upgrade as normal.

### Internal-only crates (unchanged)

- `crates/test-watchdog/` — Windows hang-dump helper (publish=false, dev-dep).
- `testbins/` — 8 test-fixture binaries (publish=false).

### Forward-looking

A future Go binding will live in its own repo (`zackees/running-process-go`) per the [Q3 resolution in #165](https://github.com/zackees/running-process/issues/165). Same pattern can support `running-process-node` or other language bindings; each gets its own crate alongside `running-process-py`.

---

Older releases are recorded in the GitHub Releases page.
