//! v1 broker protocol module.
//!
//! Phase 0 of #228 introduced the prost-generated wire types from
//! `proto/broker_v1_*.proto`. Phase 1 (#230) adds the framing
//! read/write helpers used by every connection.
//!
//! All three .proto files share the `running_process.broker.v1`
//! package, so prost-build emits a single Rust module containing
//! every message and enum (Frame, Hello, HelloReply, Refused,
//! Negotiated, AdminRequest, AdminReply, CacheManifest, ServiceDefinition,
//! LifecycleEvent, ...).
//! The prost-generated types are re-exported at the top of this
//! module so existing call sites importing them under
//! `running_process::broker::protocol::*` keep working.

pub use running_process_protocol::broker::v1::*;

/// Compatibility path for the canonical buffer-level v1 codec.
pub mod frame_ext {
    pub use crate::frame_v1::{
        encode_framed, try_decode_framed, DecodedFramed, FRAME_HEADER_BYTES,
    };
}

/// Compatibility path for the canonical stream-level v1 framing helpers.
pub mod framing {
    pub use crate::frame_v1::{
        read_frame, read_frame_with_cap, write_frame, FramingError, ENVELOPE_VERSION,
        MAX_FRAME_BYTES, MAX_HELLO_BYTES,
    };
}

/// Compatibility path for the canonical v1 payload-protocol registry.
pub mod registry {
    pub use crate::frame_v1::registry::*;
    pub use crate::frame_v1::FRAMING_VERSION_V1;
}
pub mod validate;

pub use crate::frame_v1::registry::{
    ADMIN_PAYLOAD_PROTOCOL, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL,
    FBUILD_PAYLOAD_PROTOCOL, HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION, SESSION_PAYLOAD_PROTOCOL,
    ZCCACHE_PAYLOAD_PROTOCOL,
};
pub use crate::frame_v1::{
    encode_framed, read_frame, read_frame_with_cap, try_decode_framed, write_frame, DecodedFramed,
    FramingError, ENVELOPE_VERSION, FRAME_HEADER_BYTES, MAX_FRAME_BYTES, MAX_HELLO_BYTES,
};
pub use running_process_protocol::EndpointNameError;
pub use validate::{validate_frame_envelope, FrameValidationError};
