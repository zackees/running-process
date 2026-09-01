//! v1 broker module — schemas are FROZEN FOREVER once v1.0 ships.
//!
//! Phase 0 of #228: this module exposes the prost-generated wire types
//! (envelope, manifest, service definition) for every later phase to
//! depend on. No consumers ship yet — Phases 1+ wire them in.
//!
//! See `proto/broker_v1_*.proto` and the parent issue for the rationale
//! behind every field number and `reserved` range.

#[cfg(feature = "client")]
pub mod adopt;
#[cfg(any(feature = "client", feature = "backend-identity"))]
pub mod backend_handle {
    pub use crate::backend_identity::handle::*;
}
#[cfg(feature = "client")]
pub mod backend_lib;
#[cfg(any(feature = "client", feature = "backend-identity"))]
pub mod backend_lifecycle;
#[cfg(any(feature = "client", feature = "backend-identity"))]
pub mod backend_sdk;
#[cfg(feature = "client")]
pub mod broker_http_discovery;
#[cfg(feature = "client")]
pub mod broker_http_port;
#[cfg(feature = "client")]
pub mod broker_http_server;
#[cfg(feature = "client")]
pub mod broker_owned_bind;
#[cfg(feature = "client")]
pub mod brokered_backend;
#[cfg(feature = "client")]
#[path = "builders_compat.rs"]
pub mod builders;
#[cfg(feature = "client")]
pub mod capabilities;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod client_v2;
#[cfg(feature = "client")]
pub mod connect_watchdog;
#[cfg(feature = "client")]
pub mod doctor;
#[cfg(feature = "client")]
pub mod fs_health;
#[cfg(feature = "client")]
pub mod get_http_endpoint_dispatch;
#[cfg(feature = "client")]
pub mod get_session_token_dispatch;
#[cfg(any(feature = "client", feature = "backend-identity"))]
#[path = "host_identity_compat.rs"]
pub mod host_identity;
#[cfg(feature = "client")]
pub mod http_endpoint_registry;
#[cfg(feature = "client")]
pub mod lifecycle;
#[cfg(feature = "client")]
#[path = "manifest_compat.rs"]
pub mod manifest;
#[cfg(any(feature = "client", feature = "backend-identity"))]
pub mod protocol;
#[cfg(feature = "client")]
pub mod protocol_v2;
#[cfg(feature = "client")]
#[path = "secure_dir_compat.rs"]
pub mod secure_dir;
#[cfg(feature = "client")]
pub mod server;
#[cfg(feature = "client")]
pub mod session_codec;
// The dumb-terminal client rides the async `SessionFrameCodec` (defined behind
// the `daemon` feature today); keep it there until that codec is hoisted to a
// client-async home.
#[cfg(feature = "daemon")]
pub mod session_client;
#[cfg(feature = "client")]
pub mod session_pump;
// The SESSION relay is a transparent byte proxy (no `SessionFrameCodec`). It
// uses Linux splice or portable buffered I/O under `client-async`, so the
// standalone async v2 broker can use it without the daemon runtime.
#[cfg(feature = "client-async")]
pub mod session_relay;
#[cfg(feature = "client")]
pub mod session_server;
// The async SESSION-lane takeover handler. On `client-async` (not `daemon`) so
// any async consumer daemon — including soldr-daemon, which enables
// `running-process/client-async` — can serve SESSION without the full
// running-process daemon runtime. `daemon::compile_session` re-exports it.
#[cfg(feature = "client-async")]
pub mod session_takeover;

/// Re-exported frozen v1 outer framing byte. The canonical definition lives
/// in [`crate::frame_v1`] so a frame-only consumer need not compile broker
/// ownership or IPC modules.
pub use crate::frame_v1::FRAMING_VERSION_V1;

/// Re-exported frozen v1 per-frame cap.
pub use crate::frame_v1::MAX_FRAME_SIZE_BYTES;

/// Re-exported frozen v1 Hello cap.
pub use crate::frame_v1::MAX_HELLO_SIZE_BYTES;

/// Upper bound on a LifecycleEvent's prost-encoded size, set to the
/// minimum POSIX `PIPE_BUF` so atomic-append into the event log is
/// guaranteed on every platform. Linux raises this to 4096 in practice,
/// but the cross-platform floor is 512.
pub const LIFECYCLE_EVENT_PIPE_BUF_FLOOR: usize = 512;
