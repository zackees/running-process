//! Buffer-level codecs for the v1 `Frame` envelope (#412).
//!
//! The generated type extensions live in `running-process-protocol`, where
//! the generated types are defined. This module retains the root crate's
//! buffered framing API.

use prost::Message;

use crate::broker::protocol::{Frame, FramingError, ENVELOPE_VERSION, MAX_FRAME_BYTES};

/// Length of the outer wire header: `[u8 framing_version][u32 LE body_len]`.
pub const FRAME_HEADER_BYTES: usize = 5;

/// Encode one `Frame` into complete wire bytes.
///
/// # Errors
///
/// [`FramingError::FrameTooLarge`] when the encoded body exceeds
/// [`MAX_FRAME_BYTES`].
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

/// One frame decoded from a byte buffer by [`try_decode_framed`].
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFramed {
    /// The decoded frame.
    pub frame: Frame,
    /// Total wire bytes the frame occupied (header + body).
    pub consumed: usize,
}

/// Incrementally decode one `Frame` from the front of `buf`.
///
/// Returns `Ok(None)` when the buffer does not yet hold a complete frame.
///
/// # Errors
///
/// Returns [`FramingError::UnsupportedFramingVersion`] for a foreign framing
/// version, [`FramingError::FrameTooLarge`] for an oversized body, and
/// [`FramingError::Decode`] for invalid prost bytes.
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
    let frame = Frame::decode(&buf[FRAME_HEADER_BYTES..total]).map_err(FramingError::Decode)?;
    Ok(Some(DecodedFramed {
        frame,
        consumed: total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::protocol::{Endpoint, EndpointNameError, FrameKind, PayloadEncoding};

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

    /// soldr#1178: restores the partial-frame half of the coverage
    /// `try_decode_framed_waits_for_complete_frames` gave before #1151 moved
    /// this module's neighbours into `running-process-protocol`. A streaming
    /// decoder that answers anything but `Ok(None)` for a prefix will either
    /// drop bytes or decode a truncated frame, and nothing else in the tree
    /// exercises the incomplete-buffer path.
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

        // A second frame's bytes must be left in the buffer for the next call:
        // `consumed` reports only the first frame, or the caller loses the
        // remainder of the stream.
        let mut two = wire.clone();
        two.extend_from_slice(&wire);
        let first = try_decode_framed(&two)
            .expect("decode")
            .expect("first frame is complete");
        assert_eq!(
            first.consumed,
            wire.len(),
            "trailing bytes after a complete frame belong to the next decode"
        );
    }

    /// soldr#1178: restores the hostile-input half of the coverage
    /// `try_decode_framed_rejects_foreign_version_and_oversize` gave. Both
    /// branches are wire-level commitments from #228 — the broker answers a
    /// foreign framing byte with `Refused{ERROR_VERSION_UNSUPPORTED}` and
    /// disconnects on an oversized length — so silently accepting either
    /// would be a protocol break, not just a missing test.
    #[test]
    fn try_decode_framed_rejects_foreign_version_and_oversize() {
        let foreign = ENVELOPE_VERSION.wrapping_add(1);
        assert!(
            matches!(
                try_decode_framed(&[foreign, 0, 0, 0, 0]),
                Err(FramingError::UnsupportedFramingVersion { got, expected })
                    if got == foreign && expected == ENVELOPE_VERSION
            ),
            "a foreign framing byte must be rejected before the length is read"
        );

        // The cap is checked against the *claimed* length, so the buffer need
        // not actually carry the bytes.
        let mut oversize = vec![ENVELOPE_VERSION];
        let claimed = u32::try_from(MAX_FRAME_BYTES).expect("cap fits u32") + 1;
        oversize.extend_from_slice(&claimed.to_le_bytes());
        assert!(
            matches!(
                try_decode_framed(&oversize),
                Err(FramingError::FrameTooLarge { body_length, cap })
                    if body_length == claimed as usize && cap == MAX_FRAME_BYTES
            ),
            "a claimed body over the cap must be rejected without buffering it"
        );
    }

    #[test]
    fn root_client_reexports_keep_endpoint_extensions_and_errors() {
        assert!(Endpoint::unix_socket("svc", "/tmp/svc.sock").is_ok());
        assert_eq!(
            Endpoint::windows_pipe("svc", r"\\.\pipe\svc-pipe"),
            Err(EndpointNameError::PrefixedPipeName {
                got: r"\\.\pipe\svc-pipe".to_owned(),
            })
        );
    }
}
