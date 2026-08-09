//! Codec-level tests for the SESSION lane (soldr#2365, slice 3a). These build
//! `SessionFrame`s synthetically — no process spawn — so they prove the
//! byte-transparency and partial-frame contract in isolation; the pump⇄codec
//! fidelity-against-oracle test lives in `session_pump/tests.rs`.

use super::{
    encode_session_frame, session_frame_to_frame, try_decode_session_frame, DecodedSessionFrame,
    SessionCodecError,
};
use crate::broker::protocol::{encode_framed, Frame, FrameKind, ZCCACHE_PAYLOAD_PROTOCOL};
use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};

fn sf(kind: session_frame::Kind) -> SessionFrame {
    SessionFrame { kind: Some(kind) }
}

fn every_variant() -> Vec<SessionFrame> {
    vec![
        sf(session_frame::Kind::Stdin(vec![0x00, 0x01, 0xfe, 0xff])),
        sf(session_frame::Kind::StdinEof(true)),
        sf(session_frame::Kind::Stdout(vec![0x00, 0xff, 0x80])),
        sf(session_frame::Kind::Stderr(b"diagnostic".to_vec())),
        sf(session_frame::Kind::Exit(SessionExit {
            code: 7,
            signal: 0,
        })),
        sf(session_frame::Kind::Exit(SessionExit {
            code: -1,
            signal: 9,
        })),
    ]
}

#[test]
fn every_variant_round_trips_through_the_byte_boundary() {
    for (seq, frame) in every_variant().into_iter().enumerate() {
        let wire = encode_session_frame(&frame, seq as u64).expect("encode");
        let decoded = try_decode_session_frame(&wire)
            .expect("decode ok")
            .expect("a complete frame");
        assert_eq!(decoded.frame, frame, "variant must survive the round trip");
        assert_eq!(
            decoded.consumed,
            wire.len(),
            "a single frame consumes exactly its wire length"
        );
    }
}

#[test]
fn frame_kind_is_derived_from_direction() {
    let inbound = [
        session_frame::Kind::Stdin(vec![1]),
        session_frame::Kind::StdinEof(true),
    ];
    for kind in inbound {
        let frame = session_frame_to_frame(&sf(kind), 1);
        assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Request));
        assert_eq!(frame.payload_protocol, super::SESSION_PAYLOAD_PROTOCOL);
    }
    let outbound = [
        session_frame::Kind::Stdout(vec![1]),
        session_frame::Kind::Stderr(vec![2]),
        session_frame::Kind::Exit(SessionExit { code: 0, signal: 0 }),
    ];
    for kind in outbound {
        let frame = session_frame_to_frame(&sf(kind), 2);
        assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Response));
    }
}

#[test]
fn request_id_carries_the_session_local_sequence() {
    let frame = session_frame_to_frame(&sf(session_frame::Kind::Stdout(vec![9])), 42);
    assert_eq!(frame.request_id, 42);
}

#[test]
fn partial_buffers_never_decode_until_complete() {
    let wire =
        encode_session_frame(&sf(session_frame::Kind::Stdout(b"chunk".to_vec())), 0).expect("enc");
    assert!(
        try_decode_session_frame(&[]).expect("empty ok").is_none(),
        "empty buffer yields no frame"
    );
    for cut in 1..wire.len() {
        assert!(
            try_decode_session_frame(&wire[..cut])
                .expect("partial ok")
                .is_none(),
            "a {cut}-byte prefix of a {}-byte frame must not decode",
            wire.len()
        );
    }
}

#[test]
fn concatenated_stream_decodes_in_order_with_exact_consume() {
    let frames = every_variant();
    let mut stream = Vec::new();
    for (seq, frame) in frames.iter().enumerate() {
        stream.extend_from_slice(&encode_session_frame(frame, seq as u64).expect("enc"));
    }

    // Feed the concatenated stream through the decode loop one byte at a time,
    // the way a real reader accumulates from a socket, to exercise every
    // partial-frame boundary in one pass.
    let mut fed = Vec::new();
    let mut cursor = 0usize;
    let mut recovered: Vec<SessionFrame> = Vec::new();
    for &byte in &stream {
        fed.push(byte);
        while let Some(DecodedSessionFrame { frame, consumed }) =
            try_decode_session_frame(&fed[cursor..]).expect("decode ok")
        {
            recovered.push(frame);
            cursor += consumed;
        }
    }
    assert_eq!(cursor, stream.len(), "the whole stream is consumed");
    assert_eq!(recovered, frames, "frames recover in order, byte-for-byte");
}

#[test]
fn a_frame_on_another_lane_is_rejected() {
    // A well-formed v1 frame, but on the zccache lane rather than SESSION.
    let foreign = encode_framed(&Frame::request(
        ZCCACHE_PAYLOAD_PROTOCOL,
        b"not ours".to_vec(),
    ))
    .expect("encode foreign");
    match try_decode_session_frame(&foreign) {
        Err(SessionCodecError::WrongProtocol { got }) => {
            assert_eq!(got, ZCCACHE_PAYLOAD_PROTOCOL)
        }
        other => panic!("expected WrongProtocol, got {other:?}"),
    }
}

#[test]
fn a_session_frame_with_no_kind_still_round_trips() {
    // The pump never emits an empty oneof, but the codec must not panic on one.
    let empty = SessionFrame { kind: None };
    let wire = encode_session_frame(&empty, 0).expect("enc");
    let decoded = try_decode_session_frame(&wire)
        .expect("dec ok")
        .expect("complete");
    assert_eq!(decoded.frame, empty);
}
