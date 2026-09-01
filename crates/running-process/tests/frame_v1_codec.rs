//! Minimal-feature contract for the frozen v1 `Frame` codec.
//!
//! This test deliberately imports only [`running_process::frame_v1`]. A
//! consumer that already owns its endpoint and payload must be able to retain
//! the v1 envelope without enabling broker IPC, identity hashing, or a
//! runtime.

#![cfg(feature = "frame-v1-codec")]

use running_process::frame_v1::{
    encode_framed, try_decode_framed, DecodedFramed, Frame, FrameKind, FramingError,
    PayloadEncoding, ENVELOPE_VERSION, FRAME_HEADER_BYTES, MAX_FRAME_BYTES,
};

running_process::register_payload_protocol! {
    /// Private-use value exercised by the frame-only macro contract.
    pub const FRAME_ONLY_TEST_PAYLOAD_PROTOCOL: u32 = 0xF412;
}

#[test]
fn frame_only_surface_is_available_without_broker_or_runtime() {
    assert_eq!(FRAME_ONLY_TEST_PAYLOAD_PROTOCOL, 0xF412);
    assert_eq!(ENVELOPE_VERSION, 1);
    assert_eq!(FRAME_HEADER_BYTES, 5);
    assert_eq!(MAX_FRAME_BYTES, 16 * 1024 * 1024);
}

#[test]
fn request_and_response_bytes_are_frozen() {
    let request = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(0x0102_0304_0506_0708);
    let request_wire = encode_framed(&request).expect("encode fixed request");
    let expected_request: &[u8] = &[
        0x01, 0x16, 0x00, 0x00, 0x00, // outer v1 header; 22-byte Frame
        0x08, 0x01, // Frame.envelope_version = 1
        0x18, 0xE3, 0xF4, 0x01, // payload_protocol = 0x7A63
        0x22, 0x04, b'p', b'i', b'n', b'g', // opaque payload
        0x28, 0x88, 0x8E, 0x98, 0xA8, 0xC0, 0xE0, 0x80, 0x81,
        0x01, // request_id = 0x0102030405060708
    ];
    assert_eq!(request_wire, expected_request);

    let response_template = Frame::request(0x7A63, Vec::new()).with_request_id(7);
    let response = Frame::response_to(&response_template, b"pong".to_vec());
    let response_wire = encode_framed(&response).expect("encode fixed response");
    let expected_response: &[u8] = &[
        0x01, 0x10, 0x00, 0x00, 0x00, // outer v1 header; 16-byte Frame
        0x08, 0x01, // Frame.envelope_version = 1
        0x10, 0x01, // Frame.kind = RESPONSE
        0x18, 0xE3, 0xF4, 0x01, // payload_protocol = 0x7A63
        0x22, 0x04, b'p', b'o', b'n', b'g', // opaque payload
        0x28, 0x07, // echoed request id
    ];
    assert_eq!(response_wire, expected_response);
}

#[test]
fn response_constructor_retains_request_id_trace_and_default_encoding() {
    let mut request = Frame::request(0x7A63, b"request".to_vec()).with_request_id(41);
    request.traceparent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".to_owned();
    request.tracestate = "vendor=one".to_owned();
    let response = Frame::response_to(&request, b"response".to_vec());

    assert_eq!(response.envelope_version, 1);
    assert_eq!(response.kind, FrameKind::Response as i32);
    assert_eq!(response.payload_protocol, request.payload_protocol);
    assert_eq!(response.request_id, request.request_id);
    assert_eq!(response.payload_encoding, PayloadEncoding::None as i32);
    assert_eq!(response.deadline_unix_ms, 0);
    assert_eq!(response.traceparent, request.traceparent);
    assert_eq!(response.tracestate, request.tracestate);
}

#[test]
fn decoder_preserves_partial_version_malformed_and_cap_contracts() {
    assert!(try_decode_framed(&[]).expect("empty is partial").is_none());
    assert!(try_decode_framed(&[ENVELOPE_VERSION])
        .expect("partial header")
        .is_none());
    assert!(try_decode_framed(&[ENVELOPE_VERSION, 1, 0, 0, 0])
        .expect("partial body")
        .is_none());

    assert!(matches!(
        try_decode_framed(&[2]),
        Err(FramingError::UnsupportedFramingVersion {
            got: 2,
            expected: ENVELOPE_VERSION
        })
    ));
    assert!(matches!(
        try_decode_framed(&[ENVELOPE_VERSION, 1, 0, 0, 0, 0xFF]),
        Err(FramingError::Decode(_))
    ));

    let mut oversized = vec![ENVELOPE_VERSION];
    oversized
        .extend_from_slice(&(u32::try_from(MAX_FRAME_BYTES).expect("u32 cap") + 1).to_le_bytes());
    assert!(matches!(
        try_decode_framed(&oversized),
        Err(FramingError::FrameTooLarge {
            body_length,
            cap: MAX_FRAME_BYTES,
        }) if body_length == MAX_FRAME_BYTES + 1
    ));
}

#[test]
fn decoder_reports_consumed_bytes_and_preserves_an_unread_suffix() {
    let frame = Frame {
        envelope_version: 1,
        kind: 97,
        payload_protocol: FRAME_ONLY_TEST_PAYLOAD_PROTOCOL,
        payload: b"payload".to_vec(),
        request_id: 9,
        payload_encoding: PayloadEncoding::Zstd as i32,
        deadline_unix_ms: 44,
        traceparent: "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".to_owned(),
        tracestate: "vendor=one".to_owned(),
    };
    let wire = encode_framed(&frame).expect("encode frame");
    let mut buffered = wire.clone();
    buffered.extend_from_slice(b"next-message");

    let DecodedFramed {
        frame: actual,
        consumed,
    } = try_decode_framed(&buffered)
        .expect("decode frame")
        .expect("complete frame");
    assert_eq!(actual, frame);
    assert_eq!(consumed, wire.len());
    assert_eq!(&buffered[consumed..], b"next-message");
}

#[cfg(feature = "client")]
#[test]
fn legacy_broker_paths_are_literal_frame_v1_aliases() {
    use std::any::TypeId;

    assert_eq!(
        TypeId::of::<Frame>(),
        TypeId::of::<running_process::broker::protocol::Frame>(),
    );
    assert_eq!(
        TypeId::of::<DecodedFramed>(),
        TypeId::of::<running_process::broker::protocol::DecodedFramed>(),
    );
    assert_eq!(
        TypeId::of::<FramingError>(),
        TypeId::of::<running_process::broker::protocol::FramingError>(),
    );

    let frame = Frame::request(0x7A63, b"same bytes".to_vec()).with_request_id(99);
    assert_eq!(
        encode_framed(&frame).expect("frame-v1 encode"),
        running_process::broker::protocol::encode_framed(&frame).expect("legacy encode"),
    );
}
