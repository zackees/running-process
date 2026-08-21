//! Linux local IPC transport mechanics.

use std::io::{self, Read, Write};
#[cfg(feature = "ipc-async")]
use std::pin::Pin;
#[cfg(feature = "ipc-async")]
use std::task::{Context, Poll};

use interprocess::local_socket::prelude::*;
#[cfg(feature = "ipc-async")]
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, PeerCreds, ToFsName};
use interprocess::TryClone;
#[cfg(feature = "ipc-async")]
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    pub fn new(path: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        name(&path)?;
        Ok(Self(path))
    }

    pub fn display(&self) -> &str {
        &self.0
    }

    pub fn retire(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn ensure_owner_private_parent(&self) -> io::Result<()> {
        prepare_owner_private_parent(&self.0)
    }

    pub fn target_exists(&self) -> io::Result<bool> {
        match std::fs::symlink_metadata(&self.0) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Allocate a unique endpoint for a caller-owned test or probe.
    pub fn test(label: &str) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest = blake3::hash(format!("{label}-{}-{nonce}", std::process::id()).as_bytes());
        Self::new(format!("/tmp/rp-ipc-{}.sock", &digest.to_hex()[..16]))
    }
}

fn name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn prepare_owner_private_parent(path: &str) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    let parent = std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "IPC endpoint has no parent"))?;
    let mut builder = std::fs::DirBuilder::new();
    match builder.mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not a real directory",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not owned by the current user",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not owner-private",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    pub user_id: String,
}

pub trait PeerIdentitySource {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity>;
}

fn peer_identity(creds: PeerCreds) -> PeerIdentity {
    PeerIdentity {
        pid: creds
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0),
        user_id: creds.euid().map(|uid| uid.to_string()).unwrap_or_default(),
    }
}

pub fn current_user_id() -> io::Result<String> {
    Ok(unsafe { libc::geteuid() }.to_string())
}

pub struct Stream(pub(crate) interprocess::local_socket::Stream);

impl std::fmt::Debug for Stream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IpcStream")
    }
}

impl Stream {
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        interprocess::local_socket::Stream::connect(name(endpoint.display())?).map(Self)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        interprocess::local_socket::traits::Stream::set_nonblocking(&self.0, nonblocking)
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.0.peer_creds().map(peer_identity)
    }

    /// Pass this accepted connection to a backend over its control stream.
    pub fn transfer_to_backend(
        &self,
        backend_control: &Self,
        _backend_endpoint: &Endpoint,
        _backend_pid: u32,
        sideband_payload: &[u8],
    ) -> Result<crate::platform::ipc::HandoffAttachment, crate::platform::ipc::HandoffTransferError>
    {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        let control_fd = match &backend_control.0 {
            interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
        };
        let connection_fd = match &self.0 {
            interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
        };
        send_connection_with_payload(control_fd, connection_fd, sideband_payload)?;
        Ok(crate::platform::ipc::HandoffAttachment::new(0, true))
    }
}

fn send_connection_with_payload(
    control_fd: std::os::fd::RawFd,
    connection_fd: std::os::fd::RawFd,
    sideband_payload: &[u8],
) -> Result<(), crate::platform::ipc::HandoffTransferError> {
    use crate::platform::ipc::{HandoffTransferError, HandoffTransferErrorKind};

    if sideband_payload.is_empty() {
        return Err(HandoffTransferError::new(
            HandoffTransferErrorKind::Failed,
            false,
            "connection transfer requires a non-empty sideband payload",
        ));
    }
    let mut payload = sideband_payload.to_vec();
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    // SAFETY: CMSG_SPACE only computes aligned storage for the supplied size.
    let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
    let control_slots = control_len.div_ceil(std::mem::size_of::<libc::cmsghdr>());
    // `Vec<cmsghdr>` guarantees the alignment required by CMSG_FIRSTHDR;
    // msg_controllen retains the exact byte length returned by CMSG_SPACE.
    let mut control = (0..control_slots)
        .map(|_| unsafe { std::mem::zeroed::<libc::cmsghdr>() })
        .collect::<Vec<_>>();
    // SAFETY: an all-zero msghdr is the documented empty initialization; the
    // live iovec and control-buffer pointers are installed immediately below.
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_len as _;

    // SAFETY: `message` points at live, correctly sized iovec/control storage;
    // the one SCM_RIGHTS payload is a libc::c_int file descriptor.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(HandoffTransferError::new(
                HandoffTransferErrorKind::Failed,
                false,
                "could not construct SCM_RIGHTS control message",
            ));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        *libc::CMSG_DATA(header).cast::<libc::c_int>() = connection_fd;
    }

    let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    // SAFETY: both descriptors are borrowed from live opaque streams for the
    // duration of this call and every msghdr pointer references live storage.
    let sent = unsafe { libc::sendmsg(control_fd, &message, flags) };
    if sent < 0 {
        let error = io::Error::last_os_error();
        let raw = error.raw_os_error();
        let kind = if error.kind() == io::ErrorKind::PermissionDenied {
            HandoffTransferErrorKind::PermissionDenied
        } else if error.kind() == io::ErrorKind::WouldBlock || raw == Some(libc::ENOBUFS) {
            HandoffTransferErrorKind::WouldBlock
        } else if matches!(
            error.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::NotConnected
        ) {
            HandoffTransferErrorKind::BackendUnavailable
        } else {
            HandoffTransferErrorKind::Failed
        };
        return Err(HandoffTransferError::new(
            kind,
            false,
            format!("SCM_RIGHTS connection transfer failed: {error}"),
        ));
    }
    if sent as usize != payload.len() {
        return Err(HandoffTransferError::new(
            HandoffTransferErrorKind::Failed,
            sent > 0,
            format!(
                "SCM_RIGHTS connection transfer was partial ({sent}/{} bytes)",
                payload.len()
            ),
        ));
    }
    Ok(())
}

impl PeerIdentitySource for Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_identity()
    }
}

impl PeerIdentitySource for interprocess::local_socket::Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_creds().map(peer_identity)
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListenerNonblockingMode {
    #[default]
    Neither,
    Accept,
    Stream,
    Both,
}

impl From<ListenerNonblockingMode> for interprocess::local_socket::ListenerNonblockingMode {
    fn from(value: ListenerNonblockingMode) -> Self {
        match value {
            ListenerNonblockingMode::Neither => Self::Neither,
            ListenerNonblockingMode::Accept => Self::Accept,
            ListenerNonblockingMode::Stream => Self::Stream,
            ListenerNonblockingMode::Both => Self::Both,
        }
    }
}

pub struct Listener(interprocess::local_socket::Listener);

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        Self::bind_with_options(endpoint, true, ListenerNonblockingMode::Neither)
    }

    pub fn bind_owner_only(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

        prepare_owner_private_parent(endpoint.display())?;
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .mode(0o600)
            .create_sync()
            .map(Self)
    }

    pub fn bind_with_options(
        endpoint: &Endpoint,
        reclaim_name: bool,
        nonblocking: ListenerNonblockingMode,
    ) -> io::Result<Self> {
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .reclaim_name(reclaim_name)
            .nonblocking(nonblocking.into())
            .create_sync()
            .map(Self)
    }

    pub fn accept(&self) -> io::Result<Stream> {
        self.0.accept().map(Stream)
    }

    pub fn set_nonblocking(&self, mode: ListenerNonblockingMode) -> io::Result<()> {
        self.0.set_nonblocking(mode.into())
    }

    pub fn do_not_reclaim_name_on_drop(&mut self) {
        self.0.do_not_reclaim_name_on_drop();
    }
}

/// A Unix-domain listener deliberately inherited by a child process.
///
/// The descriptor and its close-on-exec state never leave this platform
/// implementation. Callers provide their product-owned environment key and
/// command, then receive/return only opaque IPC values.
pub struct InheritedListener {
    listener: interprocess::os::unix::uds_local_socket::Listener,
}

impl InheritedListener {
    pub fn supported() -> bool {
        true
    }

    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::unix::uds_local_socket::Listener as UdsListener;

        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .create_sync_as::<UdsListener>()
            .map(|listener| Self { listener })
    }

    pub fn prepare(&self, command: &mut std::process::Command, env_key: &str) -> io::Result<()> {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        let fd = self.listener.as_fd();
        clear_cloexec(&fd)?;
        command.env(env_key, fd.as_raw_fd().to_string());
        Ok(())
    }

    pub fn disown_endpoint(&mut self) {
        use interprocess::local_socket::traits::Listener as _;

        self.listener.do_not_reclaim_name_on_drop();
    }

    pub fn recover_from_env(env_key: &str) -> io::Result<Option<Listener>> {
        let Some(raw) = std::env::var_os(env_key) else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy();
        let fd = parse_descriptor(env_key, &raw)?;
        if !is_listening_socket(fd)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{env_key}={fd} does not name a listening socket"),
            ));
        }
        use interprocess::os::unix::uds_local_socket::Listener as UdsListener;
        use std::os::fd::{FromRawFd as _, OwnedFd};
        // SAFETY: the descriptor was validated as a live stream listener and
        // is inherited into this fresh process descriptor table.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Some(Listener(UdsListener::from(owned).into())))
    }
}

fn parse_descriptor(env_key: &str, raw: &str) -> io::Result<i32> {
    let fd: i32 = raw.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{env_key}={raw:?} is not a descriptor number"),
        )
    })?;
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{env_key}={fd} is not a valid descriptor"),
        ));
    }
    Ok(fd)
}

fn clear_cloexec(fd: &std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is borrowed from a live listener for both operations.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; only FD_CLOEXEC is cleared from the returned flags.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn socket_option(fd: i32, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are correctly sized writable locals.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(len),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn is_listening_socket(fd: i32) -> io::Result<bool> {
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Ok(false);
    }
    match socket_option(fd, libc::SO_ACCEPTCONN) {
        Ok(listening) => Ok(listening != 0),
        Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => Ok(true),
        Err(error) => Err(error),
    }
}

#[cfg(feature = "ipc-async")]
pub struct AsyncStream(pub(crate) interprocess::local_socket::tokio::Stream);

#[cfg(feature = "ipc-async")]
impl AsyncStream {
    pub async fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        interprocess::local_socket::tokio::Stream::connect(name(endpoint.display())?)
            .await
            .map(Self)
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.0.peer_creds().map(peer_identity)
    }
}

#[cfg(feature = "ipc-async")]
impl PeerIdentitySource for AsyncStream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_identity()
    }
}

#[cfg(feature = "ipc-async")]
impl PeerIdentitySource for interprocess::local_socket::tokio::Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_creds().map(peer_identity)
    }
}

#[cfg(feature = "ipc-async")]
impl AsyncRead for AsyncStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(context, buffer)
    }
}

#[cfg(feature = "ipc-async")]
impl AsyncWrite for AsyncStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(context)
    }
}

#[cfg(feature = "ipc-async")]
pub trait IntoAsyncStream {
    fn into_async_stream(self) -> AsyncStream;
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncStream for AsyncStream {
    fn into_async_stream(self) -> AsyncStream {
        self
    }
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncStream for interprocess::local_socket::tokio::Stream {
    fn into_async_stream(self) -> AsyncStream {
        AsyncStream(self)
    }
}

#[cfg(feature = "ipc-async")]
pub struct AsyncListener(interprocess::local_socket::tokio::Listener);

#[cfg(feature = "ipc-async")]
impl AsyncListener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .create_tokio()
            .map(Self)
    }

    pub fn bind_owner_only(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

        prepare_owner_private_parent(endpoint.display())?;
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .mode(0o600)
            .create_tokio()
            .map(Self)
    }

    pub async fn accept(&self) -> io::Result<AsyncStream> {
        self.0.accept().await.map(AsyncStream)
    }

    pub fn do_not_reclaim_name_on_drop(&mut self) {
        self.0.do_not_reclaim_name_on_drop();
    }
}

#[cfg(all(test, feature = "ipc-async"))]
mod security_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Endpoint, IntoAsyncListener, IntoAsyncStream, Listener};

    #[test]
    fn legacy_async_listener_keeps_its_conversion_contract() {
        fn accepts<T: IntoAsyncListener>() {}
        accepts::<interprocess::local_socket::tokio::Listener>();
    }

    #[test]
    fn legacy_async_stream_keeps_its_conversion_contract() {
        fn accepts<T: IntoAsyncStream>() {}
        accepts::<interprocess::local_socket::tokio::Stream>();
    }

    #[test]
    fn sync_owner_only_security_sets_socket_mode_0600() {
        let directory = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private tempdir permissions");
        let endpoint = Endpoint::new(
            directory
                .path()
                .join("sync-owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");
        let listener = Listener::bind_owner_only(&endpoint).expect("bind endpoint");
        let mode = std::fs::metadata(endpoint.display())
            .expect("endpoint metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[tokio::test]
    async fn owner_only_security_sets_socket_mode_0600() {
        let directory = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private tempdir permissions");
        let endpoint = Endpoint::new(
            directory
                .path()
                .join("owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");
        endpoint.retire().expect("retire absent endpoint");
        let listener = super::AsyncListener::bind_owner_only(&endpoint).expect("bind endpoint");
        let mode = std::fs::metadata(endpoint.display())
            .expect("endpoint metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[test]
    fn owner_only_security_rejects_without_mutating_a_shared_parent() {
        let directory = tempfile::tempdir().expect("private tempdir");
        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).expect("shared directory");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755))
            .expect("shared permissions");
        let endpoint = Endpoint::new(
            shared
                .join("owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");

        let error = match super::AsyncListener::bind_owner_only(&endpoint) {
            Ok(_) => panic!("shared parent must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let mode = std::fs::metadata(&shared)
            .expect("shared metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }
}

#[cfg(feature = "ipc-async")]
pub trait IntoAsyncListener {
    fn into_async_listener(self) -> AsyncListener;
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncListener for AsyncListener {
    fn into_async_listener(self) -> AsyncListener {
        self
    }
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncListener for interprocess::local_socket::tokio::Listener {
    fn into_async_listener(self) -> AsyncListener {
        AsyncListener(self)
    }
}
