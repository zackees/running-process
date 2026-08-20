# running-process-platform-internal

Internal, published host-mechanics boundary for running-process.  Its public
surface is intentionally small; product policy remains in the caller crates.

Deprecated 4.x PTY trait accessors remain source-compatible until 5.0, but
platform mechanics never consume their primitive values. Native PTY control
state must stay captured inside concrete operations; the neutral facade may not
introduce descriptor/HANDLE payload structs or tokens.

`src/lib.rs` is the only host selector.  Neutral capability indexes live in
`src/platform/`; their private Windows, Linux, and macOS implementations are
selected behind that boundary.
