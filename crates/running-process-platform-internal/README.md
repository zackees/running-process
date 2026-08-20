# running-process-platform-internal

Internal, published host-mechanics boundary for running-process.  Its public
surface is intentionally small; product policy remains in the caller crates.

Deprecated 4.x PTY trait accessors remain source-compatible until 5.0, but
platform mechanics never consume their primitive values. Native PTY control
state must stay captured inside concrete operations; the neutral facade may not
introduce descriptor/HANDLE payload structs or tokens.

The established 4.x broker post-Hello callback likewise remains
source-compatible until 5.0. A hidden crate-root adapter may convert an opaque
IPC stream only at that public callback boundary; the neutral `platform::ipc`
facade never exposes the concrete stream type, and new callers must use the
opaque transport.

`src/lib.rs` is the only host selector.  Neutral capability indexes live in
`src/platform/`; their private Windows, Linux, and macOS implementations are
selected behind that boundary.
