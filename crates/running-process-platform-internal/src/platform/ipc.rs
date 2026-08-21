//! Local endpoint, listener, connection, peer, handoff, and security primitives.
//!
//! Endpoint strings and protocol policy remain with callers. These opaque
//! values own the selected host transport so callers never name Unix sockets,
//! Windows named pipes, `interprocess` types, file descriptors, or handles.

#[cfg(feature = "ipc")]
pub use crate::{
    ipc_current_user_id as current_user_id, IpcEndpoint as Endpoint,
    IpcInheritedListener as InheritedListener, IpcListener as Listener,
    IpcListenerNonblockingMode as ListenerNonblockingMode, IpcPeerIdentity as PeerIdentity,
    IpcPeerIdentitySource as PeerIdentitySource, IpcStream as Stream,
};

/// Opaque platform attachment created while transferring an accepted IPC
/// connection to a backend process.
///
/// On Windows this owns the handle-table value that must be carried by the
/// caller's existing protocol. On Unix the descriptor travels out-of-band via
/// `SCM_RIGHTS`. Native handle and descriptor values never cross the facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffAttachment {
    protocol_value: u64,
    backend_may_adopt_before_offer: bool,
}

impl HandoffAttachment {
    pub(crate) fn new(protocol_value: u64, backend_may_adopt_before_offer: bool) -> Self {
        Self {
            protocol_value,
            backend_may_adopt_before_offer,
        }
    }

    /// Append this attachment's opaque value as an unsigned protobuf varint.
    ///
    /// The caller owns the wire envelope while this facade retains ownership
    /// of the native value and its representation.
    pub fn append_unsigned_varint(self, output: &mut Vec<u8>) {
        let mut value = self.protocol_value;
        while value >= 0x80 {
            output.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        output.push(value as u8);
    }

    /// Whether the backend may adopt the connection before its offer arrives.
    ///
    /// Unix transfers the descriptor and token together in the sideband
    /// message, while Windows requires the later offer to identify the
    /// duplicated handle. Callers use this transport fact to make their own
    /// proxy-fallback ownership decision without selecting a host.
    pub fn backend_may_adopt_before_offer(self) -> bool {
        self.backend_may_adopt_before_offer
    }
}

/// Result of enforcing owner-private permissions on a local IPC directory.
#[cfg(feature = "ipc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPrivateDirectoryOutcome {
    /// The existing directory already had the complete host policy.
    AlreadyPrivate,
    /// Permissions were applied or repaired.
    Hardened,
}

/// Create a directory and enforce the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn ensure_owner_private_directory(
    path: &std::path::Path,
) -> std::io::Result<OwnerPrivateDirectoryOutcome> {
    crate::ipc_ensure_owner_private_directory(path)
}

/// Return whether a directory has the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn owner_private_directory(path: &std::path::Path) -> std::io::Result<bool> {
    crate::ipc_owner_private_directory(path)
}

/// Host-neutral classification of a failed connection-transfer primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffTransferErrorKind {
    Unsupported,
    PermissionDenied,
    BackendUnavailable,
    WouldBlock,
    Failed,
}

/// Failure from the platform-owned connection-transfer primitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffTransferError {
    kind: HandoffTransferErrorKind,
    may_have_reached_backend: bool,
    detail: String,
}

impl HandoffTransferError {
    pub(crate) fn new(
        kind: HandoffTransferErrorKind,
        may_have_reached_backend: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            may_have_reached_backend,
            detail: detail.into(),
        }
    }

    /// Return the policy-neutral failure category.
    pub fn kind(&self) -> HandoffTransferErrorKind {
        self.kind
    }

    /// Whether the backend may already own a duplicated connection.
    pub fn may_have_reached_backend(&self) -> bool {
        self.may_have_reached_backend
    }
}

impl std::fmt::Display for HandoffTransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for HandoffTransferError {}

/// Resolve a broker endpoint name using selected-host path and pipe rules.
#[cfg(feature = "ipc")]
pub fn broker_endpoint_name(bare_name: &str, path_scoped: bool) -> std::io::Result<String> {
    crate::IpcBrokerEndpointName(bare_name, path_scoped)
}

#[cfg(feature = "ipc-async")]
pub use crate::{
    IpcAsyncListener as AsyncListener, IpcAsyncStream as AsyncStream,
    IpcIntoAsyncListener as IntoAsyncListener, IpcIntoAsyncStream as IntoAsyncStream,
};

#[cfg(all(test, feature = "ipc"))]
mod tests {
    use std::io::{Read, Write};

    use super::{
        current_user_id, ensure_owner_private_directory, owner_private_directory, Endpoint,
        HandoffAttachment, Listener, Stream,
    };

    #[test]
    fn ensure_private_dir_passes_private_check() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("private");
        ensure_owner_private_directory(&path).expect("harden directory");
        assert!(owner_private_directory(&path).expect("inspect directory"));
    }

    #[test]
    fn handoff_attachment_can_be_encoded_without_exposing_its_value() {
        let mut encoded = Vec::new();
        HandoffAttachment::new(300, false).append_unsigned_varint(&mut encoded);
        assert_eq!(encoded, [0xac, 0x02]);
    }

    #[test]
    fn handoff_attachment_reports_pre_offer_adoption_semantics() {
        assert!(HandoffAttachment::new(0, true).backend_may_adopt_before_offer());
        assert!(!HandoffAttachment::new(0, false).backend_may_adopt_before_offer());
    }

    #[test]
    fn endpoint_lifecycle_mechanics_are_facade_owned() {
        let endpoint = Endpoint::test("lifecycle").expect("test endpoint");
        endpoint.retire().expect("retire absent endpoint");

        let listener = Listener::bind(&endpoint).expect("bind endpoint");

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[test]
    fn sync_bind_accept_connect_and_peer_identity_round_trip() {
        let endpoint = Endpoint::test("sync-roundtrip").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write response");
        });

        let mut client = Stream::connect(&endpoint).expect("connect");
        client.write_all(b"ping").expect("write request");
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).expect("read response");
        assert_eq!(&response, b"pong");
        server.join().expect("server thread");
    }

    #[cfg(feature = "ipc-async")]
    #[tokio::test]
    async fn async_bind_accept_connect_and_peer_identity_round_trip() {
        use super::{AsyncListener, AsyncStream};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let endpoint = Endpoint::test("async-roundtrip").expect("test endpoint");
        let listener = AsyncListener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.expect("write response");
        });

        let mut client = AsyncStream::connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write request");
        let mut response = [0_u8; 4];
        client
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"pong");
        server.await.expect("server task");
    }
}
