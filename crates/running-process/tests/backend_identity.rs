//! Minimal-feature contract for direct backend identity probing.
//!
//! This test deliberately imports only `running_process::backend_identity`.
//! A consumer that needs to identify a daemon sharing its own endpoint must
//! not need the broker client, its configuration/CLI surface, or an async
//! runtime.

#![cfg(feature = "backend-identity")]

use std::path::PathBuf;

use running_process::backend_identity::{
    encode_framed, endpoint_probe_request_from_frame, endpoint_probe_response_frame,
    try_decode_framed, BackendEndpointMux, BackendHandle, DaemonProcess, Endpoint, Frame,
    LegacyClassification, MuxError, MuxPoll, PayloadEncoding,
    BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, ENVELOPE_VERSION, MAX_FRAME_BYTES,
};

const TEST_PAYLOAD_PROTOCOL: u32 = 0xF412;

fn daemon() -> DaemonProcess {
    DaemonProcess {
        pid: 73,
        exe_path: PathBuf::from("daemon-image"),
        exe_hash: [0xA5; 32],
        legacy_exe_sha256: [0x5A; 32],
        boot_id: "boot-test".to_owned(),
        ipc_endpoint: Endpoint {
            namespace_id: "test-namespace".to_owned(),
            path: "test-daemon-endpoint".to_owned(),
        },
        started_at_unix_ms: 17,
        idle_timeout_secs: Some(30),
    }
}

fn mux() -> BackendEndpointMux<impl Fn(&[u8]) -> LegacyClassification> {
    BackendEndpointMux::new(daemon(), &[TEST_PAYLOAD_PROTOCOL], |bytes| {
        if bytes.is_empty() {
            LegacyClassification::NeedMoreBytes
        } else if bytes.len() >= 8
            && u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) == 15
        {
            LegacyClassification::Legacy
        } else {
            LegacyClassification::NotLegacy
        }
    })
}

#[test]
fn direct_surface_exposes_probe_and_no_broker_client_is_needed() {
    let _ = BackendHandle::probe as fn(&Endpoint, &DaemonProcess) -> Option<BackendHandle>;
    let _ = DaemonProcess::current_process;
}

#[test]
fn probe_identity_keeps_the_legacy_sha256_tag_three_bytes() {
    let daemon = daemon();
    let mut actual = Vec::new();
    daemon
        .encode_probe_identity(&mut actual)
        .expect("encode probe identity");

    // This is a literal v1 DaemonProcess message, not `to_proto()` derived
    // from the implementation under test. Its final tag-3 payload is the
    // SHA-256 compatibility field consumed by pre-BLAKE3 stable brokers.
    const IDENTITY_GOLDEN: &[u8] = &[
        0x08, 0x49, // pid = 73
        0x12, 0x0C, b'd', b'a', b'e', b'm', b'o', b'n', b'-', b'i', b'm', b'a', b'g', b'e', 0x22,
        0x26, // endpoint
        0x0A, 0x0E, b't', b'e', b's', b't', b'-', b'n', b'a', b'm', b'e', b's', b'p', b'a', b'c',
        b'e', 0x12, 0x14, b't', b'e', b's', b't', b'-', b'd', b'a', b'e', b'm', b'o', b'n', b'-',
        b'e', b'n', b'd', b'p', b'o', b'i', b'n', b't', 0x28, 0x11, // started = 17
        0x32, 0x09, b'b', b'o', b'o', b't', b'-', b't', b'e', b's', b't', 0x38, 0x1E, 0x42, 0x06,
        b'b', b'l', b'a', b'k', b'e', b'3', 0x4A, 0x20, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5,
        0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5,
        0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0xA5, 0x1A,
        0x20, // reserved historical SHA-256 field 3
        0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A,
        0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A,
        0x5A, 0x5A,
    ];
    assert_eq!(
        actual, IDENTITY_GOLDEN,
        "stable probes retain the v1 tag-3 payload"
    );

    let nonce = [0x09; 32];
    let request =
        Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce.to_vec()).with_request_id(7);
    let parsed = endpoint_probe_request_from_frame(&request).expect("valid fixed probe request");
    let response = endpoint_probe_response_frame(&parsed, &daemon);
    let wire = encode_framed(&response).expect("frame probe response");

    // Frozen outer v1 prefix: framing byte, 192-byte LE body, response
    // envelope, 0xB232 varint, then a 179-byte nonce+identity payload.
    let mut expected = vec![
        0x01, 0xC0, 0x00, 0x00, 0x00, 0x08, 0x01, 0x10, 0x01, 0x18, 0xB2, 0xE4, 0x02, 0x22, 0xB3,
        0x01,
    ];
    expected.extend_from_slice(&nonce);
    expected.extend_from_slice(IDENTITY_GOLDEN);
    expected.extend_from_slice(&[0x28, 0x07]);
    assert_eq!(wire, expected, "probe reply retains the frozen v1 envelope");
}

#[test]
fn mux_preserves_partial_legacy_probe_payload_and_fatal_frame_contracts() {
    let mux = mux();

    assert!(matches!(mux.poll(&[]), Ok(MuxPoll::NeedMoreBytes)));
    assert!(matches!(
        mux.poll(&[ENVELOPE_VERSION, 0, 0]),
        Ok(MuxPoll::NeedMoreBytes)
    ));

    let mut legacy = 257_u32.to_le_bytes().to_vec();
    legacy.extend_from_slice(&15_u32.to_le_bytes());
    assert_eq!(legacy[0], ENVELOPE_VERSION);
    assert!(matches!(mux.poll(&legacy), Ok(MuxPoll::Legacy)));

    let nonce = [9_u8; 32];
    let probe =
        Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce.to_vec()).with_request_id(7);
    let wire = encode_framed(&probe).expect("encode probe");
    assert!(matches!(
        mux.poll(&wire[..wire.len() - 1]),
        Ok(MuxPoll::NeedMoreBytes)
    ));
    let MuxPoll::ProbeAnswered { reply, consumed } = mux.poll(&wire).expect("answer probe") else {
        panic!("expected identity-probe reply");
    };
    assert_eq!(consumed, wire.len());
    let reply = try_decode_framed(&reply)
        .expect("decode probe reply")
        .expect("complete probe reply")
        .frame;
    assert_eq!(reply.request_id, 7);
    assert_eq!(
        reply.payload_protocol,
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL
    );
    assert_eq!(&reply.payload[..32], &nonce);

    let payload = Frame::request(TEST_PAYLOAD_PROTOCOL, b"payload".to_vec()).with_request_id(8);
    let payload_wire = encode_framed(&payload).expect("encode payload");
    let MuxPoll::Payload { frame, consumed } = mux.poll(&payload_wire).expect("pass payload")
    else {
        panic!("expected consumer payload");
    };
    assert_eq!(frame, payload);
    assert_eq!(consumed, payload_wire.len());

    let malformed = Frame::request(
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        vec![0; nonce.len() - 1],
    );
    let malformed_wire = encode_framed(&malformed).expect("encode malformed probe");
    assert!(matches!(
        mux.poll(&malformed_wire),
        Err(MuxError::MalformedProbe(_))
    ));

    let mut oversized = vec![ENVELOPE_VERSION];
    oversized.extend_from_slice(&(u32::try_from(MAX_FRAME_BYTES).unwrap() + 1).to_le_bytes());
    assert!(matches!(mux.poll(&oversized), Err(MuxError::Framing(_))));
}

#[test]
fn old_sidecar_json_without_legacy_sha256_loads_as_zeroes() {
    let directory = tempfile::tempdir().expect("temporary identity directory");
    let path = directory.path().join("daemon-identity.json");
    let original = daemon();
    let mut json = serde_json::to_value(&original).expect("serialize identity");
    assert!(json
        .as_object_mut()
        .expect("identity object")
        .remove("legacy_exe_sha256")
        .is_some());
    std::fs::write(
        &path,
        serde_json::to_vec(&json).expect("encode old sidecar"),
    )
    .expect("write old sidecar");

    let restored = running_process::backend_identity::read_daemon_identity_file(&path)
        .expect("old sidecar remains tolerantly readable");
    assert_eq!(restored.legacy_exe_sha256, [0; 32]);
    assert_eq!(restored.exe_hash, original.exe_hash);
    assert_eq!(restored.ipc_endpoint, original.ipc_endpoint);

    let strict = running_process::backend_identity::try_read_daemon_identity_file(&path)
        .expect("strict read accepts structurally compatible old JSON")
        .expect("old sidecar exists");
    assert_eq!(strict, restored);
}

#[test]
fn identity_sidecar_cleanup_removes_the_same_tolerant_identity_file() {
    let directory = tempfile::tempdir().expect("temporary identity directory");
    let path = directory.path().join("daemon-identity.json");
    running_process::backend_identity::write_daemon_identity_file(&path, &daemon())
        .expect("write identity sidecar");
    assert!(path.exists(), "precondition: sidecar was written");

    running_process::backend_identity::remove_daemon_identity_file(&path);
    assert_eq!(
        running_process::backend_identity::read_daemon_identity_file(&path),
        None,
        "cleanup restores the tolerant absent-sidecar state"
    );
}

#[test]
fn payload_encoding_is_not_rewritten_before_consumer_dispatch() {
    let mux = mux();
    let mut frame = Frame::request(TEST_PAYLOAD_PROTOCOL, b"opaque".to_vec());
    frame.payload_encoding = PayloadEncoding::Zstd as i32;
    let wire = encode_framed(&frame).expect("encode opaque frame");
    let MuxPoll::Payload { frame: actual, .. } = mux.poll(&wire).expect("poll opaque frame") else {
        panic!("expected payload");
    };
    assert_eq!(actual.payload_encoding, PayloadEncoding::Zstd as i32);
}
