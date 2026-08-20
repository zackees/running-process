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

    #[cfg(test)]
    pub fn test(label: &str) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new(
            std::env::temp_dir()
                .join(format!("rp-ipc-{label}-{}-{nonce}.sock", std::process::id()))
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    pub user_id: String,
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
pub struct AsyncListener(interprocess::local_socket::tokio::Listener);

#[cfg(feature = "ipc-async")]
impl AsyncListener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        ListenerOptions::new()
            .name(name(endpoint.display())?)
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
