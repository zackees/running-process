//! Windows local IPC transport mechanics.

use std::io::{self, Read, Write};
#[cfg(feature = "ipc-async")]
use std::pin::Pin;
#[cfg(feature = "ipc-async")]
use std::task::{Context, Poll};

use interprocess::local_socket::prelude::*;
#[cfg(feature = "ipc-async")]
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, PeerCreds, ToNsName};
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
        Ok(())
    }

    #[cfg(test)]
    pub fn test(label: &str) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new(format!(
            r"\\.\pipe\rp-ipc-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}

fn name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    path.strip_prefix(r"\\.\pipe\")
        .unwrap_or(path)
        .to_ns_name::<GenericNamespaced>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    pub user_id: String,
}

fn peer_identity(creds: PeerCreds) -> PeerIdentity {
    let pid = creds.pid().unwrap_or(0);
    PeerIdentity {
        pid,
        user_id: if pid == 0 {
            String::new()
        } else {
            process_user_sid(pid).unwrap_or_default()
        },
    }
}

fn process_user_sid(pid: u32) -> io::Result<String> {
    let bytes = process_user_sid_bytes(pid)?;
    let mut out = String::with_capacity("windows-sid:".len() + bytes.len() * 2);
    out.push_str("windows-sid:");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

fn process_user_sid_bytes(pid: u32) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let process = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(process.0, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut required = 0;
        let _ = GetTokenInformation(
            token.0,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u8; required as usize];
        let queried = GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        );
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Err(io::Error::other("invalid Windows SID"));
        }
        let len = GetLengthSid(sid) as usize;
        if len == 0 || len > 1024 {
            return Err(io::Error::other("implausible Windows SID length"));
        }
        Ok(std::slice::from_raw_parts(sid.cast::<u8>(), len).to_vec())
    }
}

#[cfg(feature = "ipc-async")]
fn owner_only_security_descriptor(
) -> io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    let sid = process_user_sid_bytes(std::process::id())?;
    let mut sid_string = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_text = unsafe {
        let mut length = 0;
        while *sid_string.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16(std::slice::from_raw_parts(sid_string, length))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        LocalFree(sid_string.cast());
        text?
    };
    let sddl = widestring::U16CString::from_str(format!("D:P(A;;GA;;;{sid_text})"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&sddl)
}

struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

pub fn current_user_id() -> io::Result<String> {
    process_user_sid(std::process::id())
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

    pub fn bind_owner_only(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;

        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .security_descriptor(owner_only_security_descriptor()?)
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

#[cfg(all(test, feature = "ipc-async"))]
mod security_tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{AsyncListener, AsyncStream, Endpoint, IntoAsyncListener};

    #[test]
    fn legacy_async_listener_keeps_its_conversion_contract() {
        fn accepts<T: IntoAsyncListener>() {}
        accepts::<interprocess::local_socket::tokio::Listener>();
    }

    #[tokio::test]
    async fn owner_only_security_allows_the_current_user() {
        let endpoint = Endpoint::test("owner-only").expect("test endpoint");
        let listener = AsyncListener::bind_owner_only(&endpoint).expect("bind endpoint");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept current user");
            stream.write_all(b"ok").await.expect("write response");
        });
        let mut client = AsyncStream::connect(&endpoint)
            .await
            .expect("current user can connect");
        let mut response = [0_u8; 2];
        client
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"ok");
        server.await.expect("server task");
    }
}
