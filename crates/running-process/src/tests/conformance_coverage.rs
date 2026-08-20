use super::*;
use crate::broker::backend_lifecycle::probe::PROBE_NONCE_BYTES;
use crate::broker::protocol::registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

const SERVED: u32 = 0xF499;

fn mux() -> BackendEndpointMux<impl Fn(&[u8]) -> LegacyClassification> {
    let endpoint = Endpoint::unix_socket("conformance-errors", "/tmp/conformance-errors.sock")
        .expect("endpoint");
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).expect("identity");
    BackendEndpointMux::new(daemon, &[SERVED], |bytes: &[u8]| match bytes.first() {
        None => LegacyClassification::NeedMoreBytes,
        Some(b'L') => LegacyClassification::Legacy,
        Some(_) => LegacyClassification::NotLegacy,
    })
}

#[test]
fn golden_helpers_report_encode_decode_trailing_and_identity_failures() {
    let frame = Frame::request(SERVED, b"payload".to_vec()).with_request_id(7);
    let encoded = encode_framed_for_golden(&frame).expect("encode");
    assert_framed_frame_matches_golden(&frame, &encoded).expect("matching golden");
    assert_framed_bytes_decode_to(&encoded, &frame).expect("matching decode");

    let mismatch = assert_framed_frame_matches_golden(&frame, b"wrong").unwrap_err();
    assert!(matches!(mismatch, ConformanceError::GoldenMismatch { .. }));
    assert!(mismatch.to_string().contains("expected (5 bytes)"));

    let malformed = assert_framed_bytes_decode_to(b"not-a-frame", &frame).unwrap_err();
    assert!(matches!(malformed, ConformanceError::GoldenMismatch { .. }));

    let short = assert_framed_bytes_decode_to(&encoded[..4], &frame).unwrap_err();
    assert!(short.to_string().contains("short read"));

    let mut trailing = encoded.clone();
    trailing.push(0);
    let trailing = assert_framed_bytes_decode_to(&trailing, &frame).unwrap_err();
    assert!(trailing.to_string().contains("trailing bytes"));

    let different = Frame::request(SERVED + 1, b"other".to_vec()).with_request_id(8);
    let identity = assert_framed_bytes_decode_to(&encoded, &different).unwrap_err();
    assert!(matches!(identity, ConformanceError::IdentityMismatch(_)));
    assert!(identity.to_string().contains("decoded frame fields differ"));
}

#[test]
fn mixed_wire_harness_reports_each_mismatched_expectation() {
    let mux = mux();

    let wrong_verdict = MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: Vec::new(),
            expect: MixedWireExpect::Legacy,
        })
        .run(&mux)
        .unwrap_err();
    assert!(matches!(
        wrong_verdict,
        ConformanceError::UnexpectedVerdict { .. }
    ));

    let frame = encode_framed(&Frame::request(SERVED, Vec::new())).expect("frame");
    let wrong_protocol = MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: frame,
            expect: MixedWireExpect::Payload {
                payload_protocol: SERVED + 1,
            },
        })
        .run(&mux)
        .unwrap_err();
    assert!(matches!(
        wrong_protocol,
        ConformanceError::UnexpectedVerdict { .. }
    ));

    let unserved = encode_framed(&Frame::request(SERVED + 1, Vec::new())).expect("frame");
    let wrong_error_text = MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: unserved.clone(),
            expect: MixedWireExpect::Error {
                error_contains: "definitely absent".into(),
            },
        })
        .run(&mux)
        .unwrap_err();
    assert!(matches!(
        wrong_error_text,
        ConformanceError::UnexpectedMuxError { .. }
    ));

    let unexpected_error = MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: unserved,
            expect: MixedWireExpect::Legacy,
        })
        .run(&mux)
        .unwrap_err();
    assert!(unexpected_error
        .to_string()
        .contains("mux returned unexpected error"));
}

#[test]
fn mixed_wire_harness_consumes_probe_payload_legacy_and_expected_error() {
    let mux = mux();
    let probe = Frame::request(
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        vec![9; PROBE_NONCE_BYTES],
    )
    .with_request_id(2);
    let payload = Frame::request(SERVED, b"ok".to_vec()).with_request_id(3);
    let unserved = Frame::request(SERVED + 1, Vec::new());

    MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: Vec::new(),
            expect: MixedWireExpect::NeedMoreBytes,
        })
        .step(MixedWireStep {
            bytes: b"Legacy".to_vec(),
            expect: MixedWireExpect::Legacy,
        })
        .step(MixedWireStep {
            bytes: encode_framed(&probe).unwrap(),
            expect: MixedWireExpect::ProbeAnswered,
        })
        .step(MixedWireStep {
            bytes: encode_framed(&payload).unwrap(),
            expect: MixedWireExpect::Payload {
                payload_protocol: SERVED,
            },
        })
        .step(MixedWireStep {
            bytes: encode_framed(&unserved).unwrap(),
            expect: MixedWireExpect::Error {
                error_contains: "Unserved".into(),
            },
        })
        .run(&mux)
        .expect("all expected verdicts");
}

#[test]
fn expectation_and_verdict_descriptions_cover_all_shapes() {
    assert_eq!(
        describe_expect(&MixedWireExpect::NeedMoreBytes),
        "NeedMoreBytes"
    );
    assert_eq!(describe_expect(&MixedWireExpect::Legacy), "Legacy");
    assert_eq!(
        describe_expect(&MixedWireExpect::ProbeAnswered),
        "ProbeAnswered"
    );
    assert!(describe_expect(&MixedWireExpect::Payload {
        payload_protocol: SERVED
    })
    .contains("0xF499"));
    assert!(describe_expect(&MixedWireExpect::Error {
        error_contains: "bad".into()
    })
    .contains("bad"));

    assert_eq!(describe_verdict(&MuxPoll::NeedMoreBytes), "NeedMoreBytes");
    assert_eq!(describe_verdict(&MuxPoll::Legacy), "Legacy");
    assert!(describe_verdict(&MuxPoll::ProbeAnswered {
        reply: Vec::new(),
        consumed: 4,
    })
    .contains("consumed=4"));
    assert!(describe_verdict(&MuxPoll::Payload {
        frame: Frame::request(SERVED, Vec::new()),
        consumed: 5,
    })
    .contains("0xF499"));
}
