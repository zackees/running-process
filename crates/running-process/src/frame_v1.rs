//! Frozen v1 `Frame` envelope primitives.
//!
//! This is the smallest running-process surface for a daemon that already
//! owns its endpoint and its application payload. It contains only the wire
//! envelope, incremental and stream codecs, and payload-protocol registration
//! checks. It deliberately does not select an IPC transport, hash a daemon
//! image, parse configuration, or start a runtime.
//!
//! The exact wire layout is `[u8 framing_version=1][u32 LE body_length][prost
//! Frame]`. These values are frozen for v1; broad broker paths re-export the
//! literal same items from this module for source and type compatibility.

use std::io::{self, Read, Write};

use prost::Message;

pub use running_process_protocol::broker::v1::{Frame, FrameKind, PayloadEncoding};

/// Framing byte for every v1 broker `Frame`.
pub const FRAMING_VERSION_V1: u8 = 1;

/// Hard ceiling on one v1 frame body.
pub const MAX_FRAME_SIZE_BYTES: usize = 16 * 1024 * 1024;

/// Hard ceiling on the initial v1 Hello envelope.
pub const MAX_HELLO_SIZE_BYTES: usize = 64 * 1024;

/// Framing byte for v1. Alias of [`FRAMING_VERSION_V1`].
pub const ENVELOPE_VERSION: u8 = FRAMING_VERSION_V1;

/// Default per-frame size cap (16 MiB). Alias of [`MAX_FRAME_SIZE_BYTES`].
pub const MAX_FRAME_BYTES: usize = MAX_FRAME_SIZE_BYTES;

/// Hello-envelope size cap (64 KiB). Alias of [`MAX_HELLO_SIZE_BYTES`].
pub const MAX_HELLO_BYTES: usize = MAX_HELLO_SIZE_BYTES;

/// Length of the outer wire header: `[u8 framing_version][u32 LE body_len]`.
pub const FRAME_HEADER_BYTES: usize = 5;

/// Errors produced while reading, writing, or incrementally decoding a v1
/// `Frame` envelope.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// Peer's framing byte did not match [`ENVELOPE_VERSION`].
    #[error("unsupported framing version: got {got}, expected {expected}")]
    UnsupportedFramingVersion {
        /// The framing byte the peer actually sent.
        got: u8,
        /// The frozen framing byte this codec expects.
        expected: u8,
    },

    /// Body length exceeds the configured per-frame cap.
    #[error("frame body too large: {body_length} bytes exceeds cap {cap}")]
    FrameTooLarge {
        /// The length announced in the four-byte little-endian header.
        body_length: usize,
        /// The cap applied by the caller or frozen default.
        cap: usize,
    },

    /// The stream ended before its complete frame arrived.
    #[error("unexpected EOF while reading frame ({context})")]
    UnexpectedEof {
        /// Which part of the frame was incomplete.
        context: &'static str,
    },

    /// Raw stream I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The complete frame body was not a valid protobuf `Frame`.
    #[error("failed to decode Frame body: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Encode one [`Frame`] into complete v1 wire bytes.
///
/// # Errors
///
/// Returns [`FramingError::FrameTooLarge`] when the encoded body exceeds the
/// frozen [`MAX_FRAME_BYTES`] cap.
pub fn encode_framed(frame: &Frame) -> Result<Vec<u8>, FramingError> {
    let body_len = frame.encoded_len();
    if body_len > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body_len,
            cap: MAX_FRAME_BYTES,
        });
    }
    let mut wire = Vec::with_capacity(FRAME_HEADER_BYTES + body_len);
    wire.push(ENVELOPE_VERSION);
    wire.extend_from_slice(&(body_len as u32).to_le_bytes());
    frame
        .encode(&mut wire)
        .expect("prost encoding into Vec cannot fail because Vec writes are infallible");
    Ok(wire)
}

/// One [`Frame`] decoded from the front of a byte buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFramed {
    /// Decoded frozen v1 envelope.
    pub frame: Frame,
    /// Total bytes occupied by the outer header and protobuf body.
    pub consumed: usize,
}

/// Incrementally decode one [`Frame`] from the front of `buf`.
///
/// Returns `Ok(None)` without consuming or interpreting a partial header or
/// body. Foreign framing versions, oversize declarations, and malformed
/// protobuf bodies are terminal errors for the current connection.
pub fn try_decode_framed(buf: &[u8]) -> Result<Option<DecodedFramed>, FramingError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != ENVELOPE_VERSION {
        return Err(FramingError::UnsupportedFramingVersion {
            got: buf[0],
            expected: ENVELOPE_VERSION,
        });
    }
    if buf.len() < FRAME_HEADER_BYTES {
        return Ok(None);
    }
    let body_len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if body_len > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body_len,
            cap: MAX_FRAME_BYTES,
        });
    }
    let total = FRAME_HEADER_BYTES + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    let frame = Frame::decode(&buf[FRAME_HEADER_BYTES..total])?;
    Ok(Some(DecodedFramed {
        frame,
        consumed: total,
    }))
}

/// Read one v1 frame body with the default 16 MiB cap.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    read_frame_with_cap(reader, MAX_FRAME_BYTES)
}

/// Read one v1 frame body, rejecting an announced size greater than
/// `max_bytes` before allocating.
pub fn read_frame_with_cap<R: Read>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, FramingError> {
    let mut version_buf = [0_u8; 1];
    read_exact_or_eof(reader, &mut version_buf, "framing byte")?;
    let version = version_buf[0];
    if version != ENVELOPE_VERSION {
        return Err(FramingError::UnsupportedFramingVersion {
            got: version,
            expected: ENVELOPE_VERSION,
        });
    }

    let mut len_buf = [0_u8; 4];
    read_exact_or_eof(reader, &mut len_buf, "body length header")?;
    let body_length = u32::from_le_bytes(len_buf) as usize;
    if body_length > max_bytes {
        return Err(FramingError::FrameTooLarge {
            body_length,
            cap: max_bytes,
        });
    }

    let mut body = vec![0_u8; body_length];
    if body_length != 0 {
        read_exact_or_eof(reader, &mut body, "frame body")?;
    }
    Ok(body)
}

/// Write one v1 frame body, flush the stream, and return the byte count.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<usize, FramingError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body.len(),
            cap: MAX_FRAME_BYTES,
        });
    }

    let body_len = body.len() as u32;
    let header = [
        ENVELOPE_VERSION,
        (body_len & 0xFF) as u8,
        ((body_len >> 8) & 0xFF) as u8,
        ((body_len >> 16) & 0xFF) as u8,
        ((body_len >> 24) & 0xFF) as u8,
    ];
    writer.write_all(&header)?;
    if !body.is_empty() {
        writer.write_all(body)?;
    }
    writer.flush()?;
    Ok(header.len() + body.len())
}

fn read_exact_or_eof<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    context: &'static str,
) -> Result<(), FramingError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(FramingError::UnexpectedEof { context })
        }
        Err(error) => Err(FramingError::Io(error)),
    }
}

/// Frozen v1 payload-protocol registry and consumer registration checks.
pub mod registry {
    /// Negotiated v1 broker protocol version carried in `Frame` protobuf
    /// fields. It is distinct from the outer [`super::FRAMING_VERSION_V1`].
    pub const PROTOCOL_VERSION: u32 = 1;

    /// Control-plane Hello/HelloReply protocol.
    pub const CONTROL_PAYLOAD_PROTOCOL: u32 = 0x00;
    /// Admin request/reply protocol.
    pub const ADMIN_PAYLOAD_PROTOCOL: u32 = 0xAD01;
    /// Same-endpoint daemon-identity nonce probe protocol.
    pub const BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL: u32 = 0xB232;
    /// Broker/backend handoff offer and acknowledgment protocol.
    pub const HANDOFF_PAYLOAD_PROTOCOL: u32 = 0xD0FF;
    /// SESSION proxy data-plane protocol.
    pub const SESSION_PAYLOAD_PROTOCOL: u32 = 0x5350;

    /// Inclusive lower bound for centrally registered consumer protocols.
    pub const CONSUMER_PAYLOAD_PROTOCOL_MIN: u32 = 0x7000;
    /// Inclusive upper bound for centrally registered consumer protocols.
    pub const CONSUMER_PAYLOAD_PROTOCOL_MAX: u32 = 0x7EFF;
    /// Inclusive lower bound for private-use protocols.
    pub const PRIVATE_USE_PAYLOAD_PROTOCOL_MIN: u32 = 0xF000;
    /// Inclusive upper bound for private-use protocols.
    pub const PRIVATE_USE_PAYLOAD_PROTOCOL_MAX: u32 = 0xFFFF;

    /// Registered consumer value for zccache's opaque Frame v1 lane.
    pub const ZCCACHE_PAYLOAD_PROTOCOL: u32 = 0x7A63;
    /// Registered consumer value for clud's opaque Frame v1 lane.
    pub const CLUD_PAYLOAD_PROTOCOL: u32 = 0x7C4C;
    /// Registered consumer value for fbuild's opaque Frame v1 lane.
    pub const FBUILD_PAYLOAD_PROTOCOL: u32 = 0x7EB1;

    /// First-party protocols that consumer values must not overlap.
    pub const FIRST_PARTY_PAYLOAD_PROTOCOLS: [u32; 4] = [
        CONTROL_PAYLOAD_PROTOCOL,
        ADMIN_PAYLOAD_PROTOCOL,
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        HANDOFF_PAYLOAD_PROTOCOL,
    ];

    /// Return whether `id` belongs to a first-party subsystem.
    pub const fn is_first_party(id: u32) -> bool {
        let mut index = 0;
        while index < FIRST_PARTY_PAYLOAD_PROTOCOLS.len() {
            if FIRST_PARTY_PAYLOAD_PROTOCOLS[index] == id {
                return true;
            }
            index += 1;
        }
        false
    }

    /// Return whether `id` is in the registered-consumer range.
    pub const fn is_registered_consumer_id(id: u32) -> bool {
        id >= CONSUMER_PAYLOAD_PROTOCOL_MIN && id <= CONSUMER_PAYLOAD_PROTOCOL_MAX
    }

    /// Return whether `id` is in the private-use range.
    pub const fn is_private_use_id(id: u32) -> bool {
        id >= PRIVATE_USE_PAYLOAD_PROTOCOL_MIN && id <= PRIVATE_USE_PAYLOAD_PROTOCOL_MAX
    }
}

pub use registry::{
    ADMIN_PAYLOAD_PROTOCOL, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CLUD_PAYLOAD_PROTOCOL,
    CONTROL_PAYLOAD_PROTOCOL, FBUILD_PAYLOAD_PROTOCOL, HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
    SESSION_PAYLOAD_PROTOCOL, ZCCACHE_PAYLOAD_PROTOCOL,
};

/// Define a consumer payload-protocol constant with compile-time range and
/// first-party-collision checks.
///
/// The authoritative allocations remain in [`registry`]; this macro verifies
/// only that a consumer's pinned literal belongs to an allowed allocation
/// range and cannot collide with a first-party protocol.
#[macro_export]
macro_rules! register_payload_protocol {
    ($(#[$meta:meta])* $vis:vis const $name:ident: u32 = $value:expr;) => {
        $(#[$meta])*
        $vis const $name: u32 = $value;

        const _: () = {
            assert!(
                !$crate::frame_v1::registry::is_first_party($name),
                concat!(
                    stringify!($name),
                    " collides with a first-party running-process payload protocol",
                ),
            );
            assert!(
                $crate::frame_v1::registry::is_registered_consumer_id($name)
                    || $crate::frame_v1::registry::is_private_use_id($name),
                concat!(
                    stringify!($name),
                    " must lie in the registered-consumer range (0x7000..=0x7EFF) ",
                    "or the private-use range (0xF000..=0xFFFF)",
                ),
            );
        };
    };
}

#[cfg(test)]
mod tests {
    use super::registry::{
        is_first_party, is_private_use_id, is_registered_consumer_id, ADMIN_PAYLOAD_PROTOCOL,
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CLUD_PAYLOAD_PROTOCOL,
        CONSUMER_PAYLOAD_PROTOCOL_MAX, CONSUMER_PAYLOAD_PROTOCOL_MIN, CONTROL_PAYLOAD_PROTOCOL,
        FBUILD_PAYLOAD_PROTOCOL, HANDOFF_PAYLOAD_PROTOCOL, PRIVATE_USE_PAYLOAD_PROTOCOL_MAX,
        PRIVATE_USE_PAYLOAD_PROTOCOL_MIN, PROTOCOL_VERSION, ZCCACHE_PAYLOAD_PROTOCOL,
    };
    use super::{
        encode_framed, try_decode_framed, Frame, FrameKind, FramingError, PayloadEncoding,
        ENVELOPE_VERSION, MAX_FRAME_BYTES,
    };

    crate::register_payload_protocol! {
        /// Registered-consumer-range example checked at compile time.
        const MACRO_CONSUMER_RANGE_EXAMPLE: u32 = 0x7001;
    }
    crate::register_payload_protocol! {
        /// Private-use-range example checked at compile time.
        const MACRO_PRIVATE_RANGE_EXAMPLE: u32 = 0xF00D;
    }

    #[test]
    fn payload_protocol_ids_are_pairwise_distinct() {
        let registered: [(u32, &str); 4] = [
            (CONTROL_PAYLOAD_PROTOCOL, "CONTROL_PAYLOAD_PROTOCOL"),
            (ADMIN_PAYLOAD_PROTOCOL, "ADMIN_PAYLOAD_PROTOCOL"),
            (
                BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL",
            ),
            (HANDOFF_PAYLOAD_PROTOCOL, "HANDOFF_PAYLOAD_PROTOCOL"),
        ];
        for (left_index, (left_id, left_name)) in registered.iter().enumerate() {
            for (right_id, right_name) in &registered[left_index + 1..] {
                assert_ne!(
                    left_id, right_id,
                    "{left_name} and {right_name} share payload-protocol id {left_id:#06X}"
                );
            }
        }
    }

    #[test]
    fn frozen_v1_wire_values() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(CONTROL_PAYLOAD_PROTOCOL, 0x00);
        assert_eq!(ADMIN_PAYLOAD_PROTOCOL, 0xAD01);
        assert_eq!(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, 0xB232);
        assert_eq!(HANDOFF_PAYLOAD_PROTOCOL, 0xD0FF);
        assert_eq!(u32::from(super::FRAMING_VERSION_V1), 1);
    }

    #[test]
    fn frozen_consumer_registry_values() {
        assert_eq!(CONSUMER_PAYLOAD_PROTOCOL_MIN, 0x7000);
        assert_eq!(CONSUMER_PAYLOAD_PROTOCOL_MAX, 0x7EFF);
        assert_eq!(PRIVATE_USE_PAYLOAD_PROTOCOL_MIN, 0xF000);
        assert_eq!(PRIVATE_USE_PAYLOAD_PROTOCOL_MAX, 0xFFFF);
        assert_eq!(ZCCACHE_PAYLOAD_PROTOCOL, 0x7A63);
        assert_eq!(CLUD_PAYLOAD_PROTOCOL, 0x7C4C);
        assert_eq!(FBUILD_PAYLOAD_PROTOCOL, 0x7EB1);
        assert!(is_first_party(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL));
        assert!(is_registered_consumer_id(ZCCACHE_PAYLOAD_PROTOCOL));
        assert!(is_private_use_id(0xF412));
    }

    #[test]
    fn register_macro_defines_usable_constants() {
        assert_eq!(MACRO_CONSUMER_RANGE_EXAMPLE, 0x7001);
        assert_eq!(MACRO_PRIVATE_RANGE_EXAMPLE, 0xF00D);
    }

    #[test]
    fn root_client_reexports_keep_frame_extensions_and_codecs() {
        let frame = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(42);
        assert_eq!(frame.kind, FrameKind::Request as i32);
        assert_eq!(frame.payload_encoding, PayloadEncoding::None as i32);

        let wire = encode_framed(&frame).expect("encode");
        let decoded = try_decode_framed(&wire)
            .expect("decode")
            .expect("complete frame");
        assert_eq!(decoded.frame, frame);
        assert_eq!(decoded.consumed, wire.len());
    }

    #[test]
    fn try_decode_framed_waits_for_complete_frames() {
        let wire = encode_framed(&Frame::request(0x7001, b"abc".to_vec())).expect("encode");

        assert!(
            try_decode_framed(&[])
                .expect("empty buffer is not an error")
                .is_none(),
            "an empty buffer must ask for more bytes, not decode"
        );
        for cut in 1..wire.len() {
            assert!(
                try_decode_framed(&wire[..cut])
                    .expect("a prefix is not an error")
                    .is_none(),
                "partial frame of {cut} of {} bytes must not decode",
                wire.len()
            );
        }

        let mut two = wire.clone();
        two.extend_from_slice(&wire);
        let first = try_decode_framed(&two)
            .expect("decode")
            .expect("first frame is complete");
        assert_eq!(first.consumed, wire.len());
    }

    #[test]
    fn try_decode_framed_rejects_foreign_version_and_oversize() {
        let foreign = ENVELOPE_VERSION.wrapping_add(1);
        assert!(matches!(
            try_decode_framed(&[foreign, 0, 0, 0, 0]),
            Err(FramingError::UnsupportedFramingVersion { got, expected })
                if got == foreign && expected == ENVELOPE_VERSION
        ));

        let mut oversize = vec![ENVELOPE_VERSION];
        let claimed = u32::try_from(MAX_FRAME_BYTES).expect("cap fits u32") + 1;
        oversize.extend_from_slice(&claimed.to_le_bytes());
        assert!(matches!(
            try_decode_framed(&oversize),
            Err(FramingError::FrameTooLarge { body_length, cap })
                if body_length == claimed as usize && cap == MAX_FRAME_BYTES
        ));
    }

    #[cfg(feature = "client")]
    #[test]
    fn root_client_reexports_keep_endpoint_extensions_and_errors() {
        use crate::broker::protocol::{Endpoint, EndpointNameError};

        assert!(Endpoint::unix_socket("svc", "/tmp/svc.sock").is_ok());
        assert_eq!(
            Endpoint::windows_pipe("svc", r"\\.\pipe\svc-pipe"),
            Err(EndpointNameError::PrefixedPipeName {
                got: r"\\.\pipe\svc-pipe".to_owned(),
            })
        );
    }
}
