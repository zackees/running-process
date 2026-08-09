//! v1 broker module — schemas are FROZEN FOREVER once v1.0 ships.
//!
//! Phase 0 of #228: this module exposes the prost-generated wire types
//! (envelope, manifest, service definition) for every later phase to
//! depend on. No consumers ship yet — Phases 1+ wire them in.
//!
//! See `proto/broker_v1_*.proto` and the parent issue for the rationale
//! behind every field number and `reserved` range.

pub mod adopt;
pub mod backend_handle;
pub mod backend_lib;
pub mod backend_lifecycle;
pub mod backend_sdk;
pub mod broker_http_discovery;
pub mod broker_http_port;
pub mod broker_http_server;
pub mod broker_owned_bind;
pub mod brokered_backend;
pub mod builders;
pub mod capabilities;
pub mod client;
pub mod client_v2;
pub mod connect_watchdog;
pub mod doctor;
pub mod fs_health;
pub mod get_http_endpoint_dispatch;
pub mod get_session_token_dispatch;
pub mod host_identity;
pub mod http_endpoint_registry;
pub mod lifecycle;
pub mod manifest;
pub mod protocol;
pub mod protocol_v2;
pub mod secure_dir;
pub mod server;
pub mod session_codec;
// The dumb-terminal client rides the async `SessionFrameCodec` (defined behind
// the `daemon` feature today); keep it there until that codec is hoisted to a
// client-async home.
#[cfg(feature = "daemon")]
pub mod session_client;
pub mod session_pump;
// The SESSION relay is a transparent byte proxy (no `SessionFrameCodec`); it
// only needs `interprocess/tokio` + `copy_bidirectional`, both now under
// `client-async`, so the standalone async v2 broker can use it without the
// daemon runtime.
#[cfg(feature = "client-async")]
pub mod session_relay;
pub mod session_server;
// The async SESSION-lane takeover handler. On `client-async` (not `daemon`) so
// any async consumer daemon — including soldr-daemon, which enables
// `running-process/client-async` — can serve SESSION without the full
// running-process daemon runtime. `daemon::compile_session` re-exports it.
#[cfg(feature = "client-async")]
pub mod session_takeover;

/// Framing byte for every v1 broker connection. Wire layout:
/// `[u8 framing_version=1][u32 LE body_length][prost body]`.
///
/// THIS BYTE is the truly-frozen-forever invariant — see #228
/// "Frozen-forever commitments" section. A v2 client connecting to a
/// v1 broker writes `[1][len][v2-shaped Hello]`; the v1 broker reads
/// the framing byte and decides whether to decode or `Refused` with
/// `ERROR_VERSION_UNSUPPORTED`.
pub const FRAMING_VERSION_V1: u8 = 1;

/// Hard ceiling on any single broker frame. Broker disconnects on
/// overflow. See #228 "Wire-level commitments".
pub const MAX_FRAME_SIZE_BYTES: usize = 16 * 1024 * 1024;

/// Hard ceiling on the Hello envelope specifically. Broker returns
/// `Refused` on overflow. See #228 "Wire-level commitments".
pub const MAX_HELLO_SIZE_BYTES: usize = 64 * 1024;

/// Upper bound on a LifecycleEvent's prost-encoded size, set to the
/// minimum POSIX `PIPE_BUF` so atomic-append into the event log is
/// guaranteed on every platform. Linux raises this to 4096 in practice,
/// but the cross-platform floor is 512.
pub const LIFECYCLE_EVENT_PIPE_BUF_FLOOR: usize = 512;
