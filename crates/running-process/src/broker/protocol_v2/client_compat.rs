//! Source-compatible broker client backed by the v2 wire (#532 criterion 5).
//!
//! The public names and one-call `AsyncBrokerSession::adopt` recipe remain the
//! frozen v1 shape used by zccache. The live session now emits its Hello via
//! [`super::super::client_v2`] and connects to the negotiated v2 backend.
//! Compatibility value and error types stay shared with v1 so callers do not
//! need an import, construction, refusal-classification, or error-handling
//! rewrite to make the wire transition.
//!
//! ## Migration contract
//!
//! Replace:
//! ```rust,ignore
//! use running_process::broker::adopt::{AdoptError, AsyncBrokerSession, OwnedConnectRequest};
//! use running_process::broker::client::{BrokerClientError, BackendConnectionRoute, RefusalKind};
//! ```
//! with:
//! ```rust,ignore
//! use running_process::broker::protocol_v2::client_compat::{
//!     AdoptError, AsyncBrokerSession, OwnedConnectRequest,
//!     BrokerClientError, BackendConnectionRoute, RefusalKind,
//! };
//! ```
//!
//! Identical Rust call shape and errors; v2 Hello and endpoint routing.

// These remain exact aliases. The canonical async session selects the
// validated v2 Hello path internally, preserving public type identity.
pub use super::super::adopt::AdoptError;

#[cfg(feature = "client-async")]
pub use super::super::adopt::{
    AsyncBrokerSession, IntoBackendIoError, OwnedBackendIo, OwnedConnectRequest,
};

// Stable decision/error vocabulary shared by both wire implementations.
pub use super::super::client::{BackendConnectionRoute, BrokerClientError, RefusalKind};

/// Classify a v2 broker error the way a v1 consumer classifies a refusal.
///
/// The first piece of the `client_compat` swap described above (#532
/// criterion 5). Consumers branch on [`RefusalKind`] today — zccache maps
/// `BrokerV2Error` in exactly one place — so the swap needs the v2 error to
/// answer the same question before anything else can move.
///
/// `None` means "not a refusal": a dial failure, a framing error, or an I/O
/// error is a different category, and flattening those into a `RefusalKind`
/// would tell a caller the broker said no when in fact it was never reached.
/// That distinction drives retry behaviour, so it is worth the `Option`.
///
/// The mapping itself is deliberately delegated to [`RefusalKind::from_code`]
/// rather than restated. A second copy of that match is a second thing to
/// keep in step, and the failure would be silent: a new `ErrorCode` would
/// classify one way through v1 and another through v2.
pub fn refusal_kind(error: &super::super::client_v2::BrokerV2Error) -> Option<RefusalKind> {
    match error {
        super::super::client_v2::BrokerV2Error::Refused { details, .. } => {
            Some(RefusalKind::from_code(details.code()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "client-async")]
    use prost::Message as _;

    /// Build a typed v2 refusal for classification parity tests.
    fn refused_v2(
        code: crate::broker::protocol::ErrorCode,
    ) -> super::super::super::client_v2::BrokerV2Error {
        let mut refused = crate::broker::protocol::Refused {
            reason: "nope".into(),
            ..Default::default()
        };
        refused.set_code(code);
        super::super::super::client_v2::BrokerV2Error::Refused {
            reason: "nope".into(),
            retry_after_ms: 0,
            details: Box::new(refused),
        }
    }

    #[test]
    fn a_v2_refusal_classifies_the_same_as_a_v1_one() {
        use crate::broker::protocol::ErrorCode;
        // The property the swap depends on: a consumer branching on
        // `RefusalKind` sees the same answer whichever client produced it.
        // Checked across the whole enum rather than one variant, because a
        // mapping that is right for `ServiceUnknown` and wrong for
        // `RateLimited` still compiles and still looks tested.
        for code in [
            ErrorCode::ErrorVersionUnsupported,
            ErrorCode::ErrorVersionBlocked,
            ErrorCode::ErrorServiceUnknown,
            ErrorCode::ErrorRateLimited,
            ErrorCode::ErrorShuttingDown,
        ] {
            assert_eq!(
                refusal_kind(&refused_v2(code)),
                Some(RefusalKind::from_code(code)),
                "v2 refusal for {code:?} classified differently from v1"
            );
        }
    }

    #[test]
    fn an_unknown_code_stays_unknown_rather_than_becoming_a_named_refusal() {
        use crate::broker::protocol::ErrorCode;
        // A code this build does not know must arrive as `Other`, not as the
        // nearest named variant. Guessing here would tell a caller the broker
        // said something specific that it did not.
        let kind = refusal_kind(&refused_v2(ErrorCode::Unspecified));
        assert_eq!(kind, Some(RefusalKind::from_code(ErrorCode::Unspecified)));
        assert!(matches!(kind, Some(RefusalKind::Other(_))));
    }

    #[test]
    fn a_transport_failure_is_not_a_refusal() {
        // The distinction that drives retry behaviour: the broker saying no
        // is not the same as never reaching it. Flattening a dial or I/O
        // error into a `RefusalKind` would report a decision the broker never
        // made.
        let io = super::super::super::client_v2::BrokerV2Error::Io(std::io::Error::other("boom"));
        assert_eq!(refusal_kind(&io), None);
    }

    #[test]
    fn v1_client_adopt_types_are_aliased_under_v2_namespace() {
        use std::any::TypeId;

        // Data and error types remain exact aliases so downstream construction
        // and matching do not change.
        assert_eq!(
            TypeId::of::<super::super::super::adopt::AdoptError>(),
            TypeId::of::<AdoptError>(),
            "AdoptError aliased"
        );
        #[cfg(feature = "client-async")]
        {
            assert_eq!(
                TypeId::of::<super::super::super::adopt::OwnedConnectRequest>(),
                TypeId::of::<OwnedConnectRequest>(),
                "OwnedConnectRequest aliased"
            );
        }

        // client: BackendConnectionRoute, BrokerClientError, RefusalKind.
        assert_eq!(
            TypeId::of::<super::super::super::client::BackendConnectionRoute>(),
            TypeId::of::<BackendConnectionRoute>(),
            "BackendConnectionRoute aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::BrokerClientError>(),
            TypeId::of::<BrokerClientError>(),
            "BrokerClientError aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::RefusalKind>(),
            TypeId::of::<RefusalKind>(),
            "RefusalKind aliased"
        );
    }

    #[cfg(feature = "client-async")]
    #[test]
    fn async_session_keeps_canonical_type_identity() {
        use std::any::TypeId;

        assert_eq!(
            TypeId::of::<super::super::super::adopt::AsyncBrokerSession>(),
            TypeId::of::<AsyncBrokerSession>(),
            "the v2 wire swap must not change public type identity"
        );
    }

    #[cfg(feature = "client-async")]
    fn test_endpoint(label: &str) -> String {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        crate::broker::server::singleton_bind::resolve_path_scoped_socket_path(&format!(
            "rp-v2-compat-{label}-{}-{nonce}",
            std::process::id()
        ))
        .expect("resolve test endpoint")
    }

    #[cfg(feature = "client-async")]
    fn bind_test_listener(endpoint: &str) -> crate::platform::ipc::Listener {
        crate::broker::server::singleton_bind::bind_singleton(endpoint).expect("bind test listener")
    }

    /// Criterion 5: the frozen `AsyncBrokerSession::adopt` call shape must
    /// actually use client_v2's Hello while preserving the backend session
    /// consumers already use. The v2 request-id prefix distinguishes that
    /// wire from the v1 alias this module used before the adapter swap.
    #[cfg(feature = "client-async")]
    #[tokio::test]
    async fn compat_adopt_speaks_v2_and_reaches_the_negotiated_backend() {
        use crate::broker::protocol::{
            hello_reply, read_frame, write_frame, Frame, FrameKind, Hello, HelloReply, Negotiated,
            PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
        };

        let broker_endpoint = test_endpoint("broker");
        let backend_endpoint = test_endpoint("backend");
        let broker_listener = bind_test_listener(&broker_endpoint);
        let backend_listener = bind_test_listener(&backend_endpoint);
        let (hello_tx, hello_rx) = std::sync::mpsc::channel();

        let backend = std::thread::spawn(move || {
            let mut stream = backend_listener.accept().expect("accept backend client");
            let bytes = read_frame(&mut stream).expect("read backend request");
            let request = Frame::decode(bytes.as_slice()).expect("decode backend request");
            let response = Frame {
                envelope_version: PROTOCOL_VERSION,
                kind: FrameKind::Response as i32,
                payload_protocol: request.payload_protocol,
                payload: b"pong".to_vec(),
                request_id: request.request_id,
                payload_encoding: PayloadEncoding::None as i32,
                deadline_unix_ms: 0,
                traceparent: String::new(),
                tracestate: String::new(),
            };
            write_frame(&mut stream, &response.encode_to_vec()).expect("write backend response");
        });

        let backend_for_broker = backend_endpoint.clone();
        let broker = std::thread::spawn(move || {
            let mut stream = broker_listener.accept().expect("accept broker client");
            let bytes = read_frame(&mut stream).expect("read Hello frame");
            let request_frame = Frame::decode(bytes.as_slice()).expect("decode request Frame");
            let hello = Hello::decode(request_frame.payload.as_slice()).expect("decode Hello");
            hello_tx
                .send(hello)
                .expect("report observed Hello contract");
            let reply = HelloReply {
                result: Some(hello_reply::Result::Negotiated(Negotiated {
                    negotiated_protocol: PROTOCOL_VERSION,
                    daemon_version: "test-daemon".into(),
                    backend_pipe: backend_for_broker,
                    ..Default::default()
                })),
            };
            let response = Frame {
                envelope_version: PROTOCOL_VERSION,
                kind: FrameKind::Response as i32,
                payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
                payload: reply.encode_to_vec(),
                request_id: request_frame.request_id,
                payload_encoding: PayloadEncoding::None as i32,
                deadline_unix_ms: 0,
                traceparent: String::new(),
                tracestate: String::new(),
            };
            write_frame(&mut stream, &response.encode_to_vec()).expect("write HelloReply frame");
        });

        let mut request =
            OwnedConnectRequest::new(&broker_endpoint, "compat-service", "1.2.3", "1.2.3");
        request.client_version = "consumer-9.8.7".into();
        request.client_lib_name = "consumer-broker-adapter".into();
        request.client_lib_version = "6.5.4".into();
        request.client_keepalive_secs = 17;
        let mut session = AsyncBrokerSession::adopt(request)
            .await
            .expect("v2-compatible adoption");
        assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);
        assert_eq!(session.endpoint(), backend_endpoint);
        let response = session
            .request(0xCAFE, b"ping".to_vec())
            .await
            .expect("backend round trip");
        assert_eq!(response.payload, b"pong");

        let hello = hello_rx.recv().expect("observed Hello contract");
        assert!(
            hello.request_id.starts_with("client_v2-compat-service-"),
            "compat adapter sent a v1 Hello request id: {:?}",
            hello.request_id
        );
        assert_eq!(hello.service_name, "compat-service");
        assert_eq!(hello.wanted_version, "1.2.3");
        assert_eq!(hello.client_version, "consumer-9.8.7");
        assert_eq!(hello.client_lib_name, "consumer-broker-adapter");
        assert_eq!(hello.client_lib_version, "6.5.4");
        assert_eq!(hello.client_keepalive_secs, 17);
        broker.join().expect("broker stub exits cleanly");
        backend.join().expect("backend stub exits cleanly");
        let _ = std::fs::remove_file(broker_endpoint);
        let _ = std::fs::remove_file(backend_endpoint);
    }
}
