//! Narrow direct-daemon identity substrate.
//!
//! A daemon that already owns its endpoint and legacy/application payloads can
//! persist its launch identity, answer the frozen v1 nonce probe on that same
//! endpoint, and later verify it without adopting the running-process broker.
//! Endpoint spelling, product naming, payload decoding, and lifecycle policy
//! deliberately remain caller-owned.

// These implementation modules deliberately live under the direct identity
// facade. The legacy v1/v2 broker namespaces below re-export their items, so
// a consumer can move to this surface without forking `TypeId`s.
#[path = "broker/backend_handle.rs"]
pub(crate) mod handle;
#[path = "broker/backend_lifecycle/identity.rs"]
pub(crate) mod identity;

pub use self::handle::BackendHandle;
#[cfg(feature = "client")]
pub use self::handle::{BackendHandleError, Connection};
pub use self::identity::{DaemonIdentityHashPolicy, DaemonProcess, IdentityError};
pub use crate::broker::backend_lifecycle::probe::{
    endpoint_probe_request_from_frame, endpoint_probe_response_frame, handle_endpoint_probe,
    read_endpoint_probe_request, write_endpoint_probe_response, EndpointProbeRequest,
    EndpointProbeServerError,
};
pub use crate::broker::backend_sdk::{
    read_daemon_identity_file, remove_daemon_identity_file, try_read_daemon_identity_file,
    write_daemon_identity_file, BackendEndpointMux, LegacyClassification, MuxError, MuxPoll,
};
pub use crate::broker::protocol::{Endpoint, EndpointNameError};
pub use crate::frame_v1::{
    encode_framed, read_frame, read_frame_with_cap, try_decode_framed, write_frame, DecodedFramed,
    Frame, FrameKind, FramingError, PayloadEncoding, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
    ENVELOPE_VERSION, FRAME_HEADER_BYTES, MAX_FRAME_BYTES, MAX_HELLO_BYTES, PROTOCOL_VERSION,
};

#[cfg(all(test, feature = "client"))]
mod type_identity_tests {
    use std::any::TypeId;

    #[test]
    fn legacy_broker_paths_are_literal_direct_identity_aliases() {
        assert_eq!(
            TypeId::of::<super::BackendHandle>(),
            TypeId::of::<crate::broker::backend_handle::BackendHandle>(),
        );
        assert_eq!(
            TypeId::of::<super::DaemonProcess>(),
            TypeId::of::<crate::broker::backend_handle::DaemonProcess>(),
        );
        assert_eq!(
            TypeId::of::<super::Connection>(),
            TypeId::of::<crate::broker::backend_handle::Connection>(),
        );
        assert_eq!(
            TypeId::of::<super::BackendHandleError>(),
            TypeId::of::<crate::broker::backend_handle::BackendHandleError>(),
        );
    }
}

#[cfg(test)]
mod direct_probe_e2e_tests {
    use std::thread;

    use super::{handle_endpoint_probe, BackendHandle, DaemonProcess, Endpoint};

    #[test]
    fn direct_identity_probe_round_trips_over_the_existing_endpoint() {
        let transport_endpoint = crate::platform::ipc::Endpoint::test("backend-identity-e2e")
            .expect("allocate test endpoint");
        let endpoint = Endpoint {
            namespace_id: "backend-identity-e2e".to_owned(),
            path: transport_endpoint.display().to_owned(),
        };
        let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30))
            .expect("current daemon identity");
        let listener = crate::platform::ipc::Listener::bind(&transport_endpoint)
            .expect("bind existing daemon endpoint");
        let responder_identity = daemon.clone();
        let responder = thread::spawn(move || {
            let mut stream = listener.accept().expect("accept identity probe");
            handle_endpoint_probe(&mut stream, &responder_identity)
                .expect("answer nonce identity probe");
        });

        let handle = BackendHandle::probe(&endpoint, &daemon)
            .expect("current daemon must verify over its existing endpoint");
        assert_eq!(handle.daemon_process, daemon);
        responder.join().expect("identity responder exits");
    }
}
