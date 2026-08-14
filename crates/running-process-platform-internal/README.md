# running-process-platform-internal

Internal, published host-mechanics boundary for running-process.  Its public
surface is intentionally small; product policy remains in the caller crates.

`src/lib.rs` is the only host selector.  Neutral capability indexes live in
`src/platform/`; their private Windows, Linux, and macOS implementations are
selected behind that boundary.
