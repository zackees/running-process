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

/// Host-neutral candidates for one endpoint address.
///
/// Product naming policy may derive both a kernel-namespace name and a
/// filesystem path. The selected transport chooses the applicable standard
/// library value without exposing that host choice to the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg(feature = "ipc")]
pub struct EndpointAddressCandidates {
    kernel_namespace: Option<String>,
    filesystem: Option<std::path::PathBuf>,
}

#[cfg(feature = "ipc")]
impl EndpointAddressCandidates {
    pub fn new(kernel_namespace: Option<String>, filesystem: Option<std::path::PathBuf>) -> Self {
        Self {
            kernel_namespace,
            filesystem,
        }
    }

    /// Select the address used by the active local IPC transport.
    pub fn select(self) -> Option<String> {
        crate::ipc_select_endpoint_address(self.kernel_namespace, self.filesystem)
    }
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

/// Compatibility path for the owner-private directory result.
#[cfg(feature = "ipc")]
pub use crate::platform::private_dir::OwnerPrivateDirectoryOutcome;

/// Create a directory and enforce the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn ensure_owner_private_directory(
    path: &std::path::Path,
) -> std::io::Result<OwnerPrivateDirectoryOutcome> {
    crate::platform::private_dir::ensure_owner_private_directory(path)
}

/// Return whether a directory has the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn owner_private_directory(path: &std::path::Path) -> std::io::Result<bool> {
    crate::platform::private_dir::owner_private_directory(path)
}

/// Whether an empty nonblocking read means "not ready yet" for this host's
/// local IPC transport rather than end-of-stream.
#[cfg(feature = "ipc")]
pub fn nonblocking_zero_read_is_pending() -> bool {
    crate::ipc_nonblocking_zero_read_is_pending()
}

/// Whether the selected local IPC transport uses filesystem endpoint names.
#[cfg(feature = "ipc")]
pub fn endpoint_is_filesystem_backed() -> bool {
    crate::ipc_endpoint_is_filesystem_backed()
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

/// The selected host's limit on a local IPC endpoint name.
///
/// Unix transports are bounded by the `sun_path` field of `sockaddr_un`;
/// Windows named pipes are bounded by `MAX_PATH` unless the long-path
/// prefix is in use. Callers use this to report a budget without naming
/// which host they are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointNameLimit {
    /// Largest endpoint name this host accepts, in bytes.
    pub max_bytes: usize,
    /// Operator-facing name of the limit, e.g. `"macOS sun_path"`.
    pub label: &'static str,
}

/// Report the selected host's endpoint-name budget.
#[cfg(feature = "ipc")]
pub fn endpoint_name_limit() -> EndpointNameLimit {
    crate::ipc_endpoint_name_limit()
}

/// A derived endpoint name that does not fit this host's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointNameTooLong {
    /// Length of the name that was derived.
    pub len: usize,
    /// Largest length that would have been accepted.
    pub max: usize,
    /// Operator-facing name of the limit that rejected it.
    pub limit_label: &'static str,
}

/// Last-resort per-user root when the host's runtime/temp variable is unset.
///
/// Deliberately not `/tmp`: a per-user directory keeps two accounts on one
/// host from colliding without naming a uid. Only reached when the platform's
/// runtime variable is missing -- cron and sessionless ssh being the realistic
/// cases.
#[cfg(feature = "ipc")]
pub(crate) fn per_user_runtime_fallback() -> std::path::PathBuf {
    dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("running-process")
        .join("broker-v2")
}

/// Canonical byte spelling of `path` for endpoint-scope identity.
///
/// Two callers naming the same installed file must hash to the same scope, so
/// the selected host decides which spelling differences are meaningless. On a
/// case-insensitive host that means folding case and separator style; on a
/// host whose paths are opaque byte strings it means the bytes as they are.
/// Callers own the hash itself, its domain separator, and its encoding.
#[cfg(feature = "ipc")]
pub fn endpoint_scope_bytes(path: &std::path::Path) -> Vec<u8> {
    crate::ipc_endpoint_scope_bytes(path)
}

/// The selected host's directory for broker-v2 runtime files and sockets.
///
/// Every host lands inside a location the OS already scopes to a single user,
/// so two accounts stay apart without a uid spelled into the path. The leaf is
/// chosen per host rather than shared: on macOS the broker's sockets live under
/// this root, and `sun_path` leaves no budget for a long one.
///
/// The directory is not created here. A caller that writes into it creates it
/// owner-only at that point; a caller that only reads treats an absent
/// directory as "nothing published", which is a normal state.
#[cfg(feature = "ipc")]
pub fn broker_v2_runtime_dir() -> std::path::PathBuf {
    crate::ipc_broker_v2_runtime_dir()
}

/// Derive the v1 broker endpoint address for `bare_name`.
///
/// The selected host owns directory placement, the leaf spelling, and the
/// length check. Callers own which bare name to ask for. The returned string
/// is the address the caller passes back to [`Endpoint::new`]; it is a
/// filesystem path where [`endpoint_is_filesystem_backed`] reports `true` and
/// a kernel-namespace name otherwise.
#[cfg(feature = "ipc")]
pub fn broker_v1_endpoint_path(bare_name: &str) -> Result<String, EndpointNameTooLong> {
    crate::ipc_broker_v1_endpoint_path(bare_name)
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
