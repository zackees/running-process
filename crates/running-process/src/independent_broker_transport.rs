//! Bounded, byte-preserving submission framing for an already-running broker.
//! Only an opaque private request path crosses this channel, never target argv.

use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"RPBRK001";
const MAX_PATH_BYTES: usize = 4096;

pub(crate) fn connect(
    socket: &Path,
    started: std::time::Instant,
    budget: std::time::Duration,
    cancellation: Option<&crate::IndependentSpawnCancellation>,
) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let bytes = socket.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if !socket.is_absolute()
        || bytes.is_empty()
        || bytes.len() >= address.sun_path.len()
        || bytes.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid broker socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, byte) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *byte as libc::c_char;
    }
    let check = || {
        if cancellation.is_some_and(|token| token.is_cancelled()) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "broker launch cancelled",
            ))
        } else if started.elapsed() >= budget {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "broker connect deadline expired",
            ))
        } else {
            Ok(())
        }
    };
    loop {
        check()?;
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
        let result = unsafe {
            libc::connect(
                descriptor.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
                    as libc::socklen_t,
            )
        };
        if result == 0 {
            return Ok(descriptor.into());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINPROGRESS) {
            loop {
                check()?;
                let mut event = libc::pollfd {
                    fd: descriptor.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut event, 1, 5) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    continue;
                }
                let mut status: libc::c_int = 0;
                let mut length = std::mem::size_of_val(&status) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        descriptor.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut status as *mut libc::c_int).cast(),
                        &mut length,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if status != 0 {
                    return Err(io::Error::from_raw_os_error(status));
                }
                return Ok(descriptor.into());
            }
        }
        if error.kind() != io::ErrorKind::WouldBlock && error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        drop(descriptor);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// One aggregate deadline across all broker reads/writes, including partial
/// progress. A stalled peer cannot refresh its allowance by trickling bytes.
pub(crate) struct DeadlineStream {
    stream: std::os::unix::net::UnixStream,
    started: std::time::Instant,
    budget: std::time::Duration,
    cancellation: Option<crate::IndependentSpawnCancellation>,
}

impl DeadlineStream {
    pub(crate) fn new(
        stream: std::os::unix::net::UnixStream,
        started: std::time::Instant,
        budget: std::time::Duration,
        cancellation: Option<crate::IndependentSpawnCancellation>,
    ) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            started,
            budget,
            cancellation,
        })
    }

    fn check(&self) -> io::Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "broker launch cancelled",
            ));
        }
        if self.started.elapsed() >= self.budget {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "broker launch deadline expired",
            ));
        }
        Ok(())
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check()
    }
}

pub(crate) fn read_greeting(reader: &mut impl Read) -> io::Result<()> {
    let mut greeting = [0; 8];
    reader.read_exact(&mut greeting)?;
    if &greeting != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incompatible independent broker",
        ));
    }
    Ok(())
}

pub(crate) fn peer_pid(stream: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != unsafe { libc::geteuid() }
        || credentials.pid <= 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker peer is not the same user",
        ));
    }
    Ok(credentials.pid as u32)
}

/// Run explicitly from an external service/container supervisor. Never called
/// by the launch client's discovery path.
pub(crate) fn serve(socket: &Path, helper: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    if !socket.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker socket must be absolute",
        ));
    }
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket directory"))?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker directory must be owner-private",
        ));
    }
    // Never unlink a pre-existing endpoint: it may belong to a live broker.
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let mut children: Vec<std::process::Child> = Vec::new();
    loop {
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
        if peer_pid(&stream).is_err() || children.len() >= 64 {
            let _ = stream.write_all(&[1]);
            continue;
        }
        if stream.write_all(MAGIC).is_err() {
            continue;
        }
        let submission = read_submission(&mut stream).and_then(|(request, timeout)| {
            if timeout == 0 || timeout > 3_600_000 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "broker launch timeout is out of range",
                ));
            }
            // Validate privacy and bounded contents before asking the helper
            // to consume them again. No target values enter argv or logs.
            let _ = crate::independent_transport::read_private_request(&request)?;
            let acknowledgement = request.with_file_name("ack");
            let child = std::process::Command::new(helper)
                .arg(&request)
                .arg(acknowledgement)
                .arg(timeout.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            Ok(child)
        });
        match submission {
            Ok(child) => {
                // On disconnect retain the supervisor: its acceptance timer
                // must kill/reap the target, not leave it orphaned by killing
                // the supervisor itself.
                let _ = stream.write_all(&[0]);
                children.push(child);
            }
            Err(_) => {
                let _ = stream.write_all(&[1]);
            }
        }
    }
}

pub(crate) fn write_submission(
    writer: &mut impl Write,
    request: &Path,
    timeout_ms: u64,
) -> io::Result<()> {
    let bytes = request.as_os_str().as_bytes();
    if !request.is_absolute()
        || bytes.is_empty()
        || bytes.len() > MAX_PATH_BYTES
        || bytes.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid broker request path",
        ));
    }
    writer.write_all(MAGIC)?;
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&timeout_ms.to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

pub(crate) fn read_submission(reader: &mut impl Read) -> io::Result<(PathBuf, u64)> {
    let mut header = [0; 20];
    reader.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported broker protocol",
        ));
    }
    let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    if length == 0 || length > MAX_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid broker path length",
        ));
    }
    let timeout = u64::from_le_bytes(header[12..20].try_into().unwrap());
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid broker path bytes",
        ));
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker path must be absolute",
        ));
    }
    Ok((path, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_and_cancellation_apply_before_io() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut stream = DeadlineStream::new(
            stream,
            std::time::Instant::now(),
            std::time::Duration::ZERO,
            None,
        )
        .unwrap();
        assert_eq!(
            stream.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let token = crate::IndependentSpawnCancellation::new();
        token.cancel();
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut stream = DeadlineStream::new(
            stream,
            std::time::Instant::now(),
            std::time::Duration::from_secs(1),
            Some(token),
        )
        .unwrap();
        assert_eq!(
            stream.write(&[0]).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn roundtrip_preserves_non_utf8_path() {
        let path = PathBuf::from(std::ffi::OsString::from_vec(
            b"/private/\xff/request".to_vec(),
        ));
        let mut bytes = Vec::new();
        write_submission(&mut bytes, &path, 1234).unwrap();
        assert_eq!(
            read_submission(&mut bytes.as_slice()).unwrap(),
            (path, 1234)
        );
    }

    #[test]
    fn rejects_unbounded_path_before_allocation() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        assert!(read_submission(&mut bytes.as_slice()).is_err());
    }
}
