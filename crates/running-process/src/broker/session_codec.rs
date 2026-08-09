//! Phase 3 SESSION-lane codec (soldr#2365, slice 3a): bridge the proxy pump's
//! `SessionFrame`s onto the v1 `Frame` envelope and back.
//!
//! The pump ([`crate::broker::session_pump`]) speaks in-memory `SessionFrame`
//! channels; this module is the byte-boundary twin that lets those frames cross
//! a real transport. Each `SessionFrame` rides exactly one `Frame` on the
//! [`SESSION_PAYLOAD_PROTOCOL`] lane, encoded with the same
//! `[u8 framing_version=1][u32 LE len][prost Frame]` wire shape every other
//! lane uses (via [`encode_framed`] / [`try_decode_framed`]).
//!
//! **Direction.** The frame kind is derived from the `SessionFrame` variant:
//! client→daemon frames (`Stdin`, `StdinEof`) are `REQUEST`, daemon→client
//! frames (`Stdout`, `Stderr`, `Exit`) are `RESPONSE`. This is a provisional
//! hint, not request/response correlation — a compile session is one long-lived
//! bidirectional exchange, so `request_id` carries a session-local **sequence
//! number** for observability, not a request/response pairing. A later slice
//! (3b) that multiplexes many sessions over one endpoint will introduce a real
//! session id; until then one transport carries one session and direction is
//! implied by which half of the duplex a frame arrives on.
//!
//! This module deliberately does **not** touch the broker socket, `FrameClient`,
//! or any OS handle: it is a pure `SessionFrame <-> bytes` codec so the fidelity
//! it guarantees (byte-transparency across partial-frame boundaries) can be
//! proven in isolation and reused by whatever transport slice 3b lands.

use prost::Message;

use crate::broker::protocol::{
    encode_framed, try_decode_framed, Frame, FrameKind, FramingError, SESSION_PAYLOAD_PROTOCOL,
};
use crate::broker::protocol_v2::{session_frame, SessionFrame};

/// The `Frame` kind a `SessionFrame` rides under, derived from its direction.
///
/// Inbound (client→daemon) stdin frames are `REQUEST`; outbound
/// (daemon→client) stdout/stderr/exit frames are `RESPONSE`. See the module
/// docs for why this is a hint rather than request/response correlation.
fn frame_kind_for(kind: &session_frame::Kind) -> FrameKind {
    match kind {
        session_frame::Kind::Stdin(_) | session_frame::Kind::StdinEof(_) => FrameKind::Request,
        session_frame::Kind::Stdout(_)
        | session_frame::Kind::Stderr(_)
        | session_frame::Kind::Exit(_) => FrameKind::Response,
    }
}

/// Wrap one `SessionFrame` in a SESSION-lane `Frame`.
///
/// The envelope carries the v1 defaults (`envelope_version`,
/// `payload_encoding = NONE`, no deadline, empty trace context) from
/// [`Frame::request`], with `payload_protocol` pinned to
/// [`SESSION_PAYLOAD_PROTOCOL`], `request_id` set to the session-local `seq`,
/// and `kind` derived from the frame's direction. A `SessionFrame` with no
/// `kind` set (an empty oneof — never produced by the pump) defaults to
/// `REQUEST`; it still round-trips, carrying an empty payload.
pub fn session_frame_to_frame(frame: &SessionFrame, seq: u64) -> Frame {
    let kind = frame
        .kind
        .as_ref()
        .map_or(FrameKind::Request, frame_kind_for);
    let mut wrapped =
        Frame::request(SESSION_PAYLOAD_PROTOCOL, frame.encode_to_vec()).with_request_id(seq);
    wrapped.kind = kind as i32;
    wrapped
}

/// Encode one `SessionFrame` to complete wire bytes
/// (`[1][u32 len][prost Frame]`), ready to write to a transport.
///
/// # Errors
///
/// [`FramingError::FrameTooLarge`] when the encoded envelope exceeds the frame
/// cap — propagated from [`encode_framed`].
pub fn encode_session_frame(frame: &SessionFrame, seq: u64) -> Result<Vec<u8>, FramingError> {
    encode_framed(&session_frame_to_frame(frame, seq))
}

/// One `SessionFrame` decoded from the front of a byte buffer, plus how many
/// wire bytes it occupied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSessionFrame {
    /// The decoded session frame.
    pub frame: SessionFrame,
    /// Total wire bytes consumed (outer header + envelope body). The caller
    /// advances its read buffer by exactly this many bytes.
    pub consumed: usize,
}

/// Incrementally decode one SESSION-lane `SessionFrame` from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a complete frame — the
/// caller reads more bytes and retries. On `Ok(Some(decoded))` the caller
/// consumes `decoded.consumed` bytes. This mirrors [`try_decode_framed`] and
/// adds the SESSION-lane payload decode on top.
///
/// # Errors
///
/// - [`SessionCodecError::Framing`] for a malformed outer frame (bad framing
///   version, oversize, or undecodable envelope).
/// - [`SessionCodecError::WrongProtocol`] when the envelope is well-formed but
///   is not on the SESSION lane — a caller multiplexing lanes must route by
///   `payload_protocol` before calling this.
/// - [`SessionCodecError::Decode`] when the envelope payload is not a valid
///   `SessionFrame`.
pub fn try_decode_session_frame(
    buf: &[u8],
) -> Result<Option<DecodedSessionFrame>, SessionCodecError> {
    let Some(decoded) = try_decode_framed(buf).map_err(SessionCodecError::Framing)? else {
        return Ok(None);
    };
    if decoded.frame.payload_protocol != SESSION_PAYLOAD_PROTOCOL {
        return Err(SessionCodecError::WrongProtocol {
            got: decoded.frame.payload_protocol,
        });
    }
    let frame = SessionFrame::decode(decoded.frame.payload.as_slice())
        .map_err(SessionCodecError::Decode)?;
    Ok(Some(DecodedSessionFrame {
        frame,
        consumed: decoded.consumed,
    }))
}

/// Errors from [`try_decode_session_frame`].
#[derive(Debug, thiserror::Error)]
pub enum SessionCodecError {
    /// The outer v1 frame was malformed (see [`FramingError`]).
    #[error(transparent)]
    Framing(FramingError),
    /// The frame decoded but was not on the SESSION lane. The value is the
    /// `payload_protocol` that was seen; the expected lane is `0x5350`.
    #[error("frame carried payload_protocol {got:#06X}, expected SESSION lane 0x5350")]
    WrongProtocol {
        /// The payload protocol the frame actually carried.
        got: u32,
    },
    /// The envelope payload was not a valid `SessionFrame`.
    #[error("failed to decode SessionFrame payload: {0}")]
    Decode(prost::DecodeError),
}

#[cfg(test)]
mod tests;
