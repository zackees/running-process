//! Unix `SCM_RIGHTS` handoff transport model.
//!
//! This module owns the broker-side `sendmsg(SCM_RIGHTS)` call used to pass an
//! already-accepted client connection into a backend process. The backend still
//! has to verify the one-time token before adopting the connection; failures
//! map into the existing silent reconnect fallback policy.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use super::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason,
    HandoffToken,
};

/// Whether this build target can eventually use Unix-domain `SCM_RIGHTS`.
pub const SCM_RIGHTS_TRANSPORT_SUPPORTED: bool = cfg!(unix);

/// Opaque raw Unix file descriptor value owned by the broker or backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnixFileDescriptor(i32);

impl UnixFileDescriptor {
    /// Build an opaque file descriptor value for transport bookkeeping.
    pub fn new(raw_fd: i32) -> Self {
        Self(raw_fd)
    }

    /// Return the raw opaque file descriptor value.
    pub fn raw(self) -> i32 {
        self.0
    }
}

/// Backend Unix-domain socket that will receive `SCM_RIGHTS` messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixHandoffSocket {
    /// Filesystem path or platform socket path for the backend handoff socket.
    pub path: PathBuf,
}

impl UnixHandoffSocket {
    /// Build a backend handoff socket descriptor.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

/// Inputs for one future `sendmsg(SCM_RIGHTS)` attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScmRightsAttempt {
    /// Broker-owned connection file descriptor to pass.
    pub fd: UnixFileDescriptor,
    /// Backend handoff socket that should receive the file descriptor.
    pub backend_socket: UnixHandoffSocket,
    /// One-time token associated with this handoff attempt.
    pub handoff_token: HandoffToken,
}

impl ScmRightsAttempt {
    /// Build typed inputs for one `SCM_RIGHTS` attempt.
    pub fn new(
        fd: UnixFileDescriptor,
        backend_socket: UnixHandoffSocket,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            fd,
            backend_socket,
            handoff_token,
        }
    }
}

/// Successful `SCM_RIGHTS` outcome once real fd passing is wired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScmRightsSuccess {
    /// File descriptor value sent to the backend.
    pub sent_fd: UnixFileDescriptor,
    /// Backend handoff socket that received the file descriptor.
    pub backend_socket: UnixHandoffSocket,
    /// One-time token paired with the sent file descriptor.
    pub handoff_token: HandoffToken,
}

impl ScmRightsSuccess {
    /// Build a typed successful handoff result.
    pub fn new(
        sent_fd: UnixFileDescriptor,
        backend_socket: UnixHandoffSocket,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            sent_fd,
            backend_socket,
            handoff_token,
        }
    }
}

/// Result returned by the future Unix transport.
pub type ScmRightsResult = Result<ScmRightsSuccess, ScmRightsError>;

/// Try to send the broker-held file descriptor to the backend handoff socket.
///
/// The sent file descriptor remains owned by the broker. The backend receives
/// a duplicate descriptor through `SCM_RIGHTS` and must verify the paired
/// [`HandoffToken`] before treating the connection as adopted.
pub fn try_send_scm_rights(attempt: &ScmRightsAttempt) -> ScmRightsResult {
    platform_try_send_scm_rights(attempt)
}

/// Send the broker-held file descriptor and token over an already-connected
/// Unix-domain handoff socket.
///
/// [`try_send_scm_rights`] dials a fresh connection per attempt; the
/// production serve path instead reuses the framed broker↔backend handoff
/// connection so the `SCM_RIGHTS` message and the [`HandoffOffer`
/// frame](crate::broker::protocol::HandoffOffer) travel over the same
/// stream. The caller keeps ownership of both descriptors.
#[cfg(unix)]
pub fn try_send_scm_rights_over(
    socket_fd: std::os::fd::RawFd,
    attempt: &ScmRightsAttempt,
) -> ScmRightsResult {
    send_fd_with_token(
        socket_fd,
        attempt.fd.raw(),
        attempt.handoff_token.as_bytes(),
        &attempt.backend_socket.path,
    )?;
    Ok(ScmRightsSuccess::new(
        attempt.fd,
        attempt.backend_socket.clone(),
        attempt.handoff_token,
    ))
}

/// Failure from a future `sendmsg(SCM_RIGHTS)` handoff attempt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScmRightsError {
    /// The current target cannot use the Unix handoff transport.
    #[error("SCM_RIGHTS handoff transport is unsupported on this platform")]
    UnsupportedPlatform,
    /// The platform denied file descriptor passing.
    #[error("permission denied passing fd {fd} to backend handoff socket {socket}")]
    PermissionDenied {
        /// File descriptor targeted by the handoff.
        fd: i32,
        /// Backend handoff socket path.
        socket: PathBuf,
    },
    /// The backend handoff socket could not be reached.
    #[error("backend handoff socket is unavailable: {socket}")]
    BackendSocketUnavailable {
        /// Backend handoff socket path.
        socket: PathBuf,
    },
    /// The nonblocking `SCM_RIGHTS` send could not complete immediately.
    #[error("SCM_RIGHTS send would block for backend handoff socket {socket}")]
    WouldBlock {
        /// Backend handoff socket path.
        socket: PathBuf,
    },
    /// The `sendmsg(SCM_RIGHTS)` call failed after connecting to the backend socket.
    #[error("SCM_RIGHTS send failed for fd {fd} to backend handoff socket {socket}")]
    SendFailed {
        /// File descriptor targeted by the handoff.
        fd: i32,
        /// Backend handoff socket path.
        socket: PathBuf,
        /// Raw OS error code returned by the platform, when available.
        raw_os_error: Option<i32>,
    },
    /// The backend did not acknowledge the passed file descriptor before the deadline.
    #[error("backend handoff socket {socket} did not acknowledge passed fd")]
    BackendAckTimeout {
        /// Backend handoff socket path.
        socket: PathBuf,
    },
}

#[cfg(unix)]
fn platform_try_send_scm_rights(attempt: &ScmRightsAttempt) -> ScmRightsResult {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(&attempt.backend_socket.path)
        .map_err(|err| socket_connect_error(&attempt.backend_socket.path, err))?;
    stream
        .set_nonblocking(true)
        .map_err(|err| socket_connect_error(&attempt.backend_socket.path, err))?;

    send_fd_with_token(
        stream.as_raw_fd(),
        attempt.fd.raw(),
        attempt.handoff_token.as_bytes(),
        &attempt.backend_socket.path,
    )?;

    Ok(ScmRightsSuccess::new(
        attempt.fd,
        attempt.backend_socket.clone(),
        attempt.handoff_token,
    ))
}

#[cfg(not(unix))]
fn platform_try_send_scm_rights(_attempt: &ScmRightsAttempt) -> ScmRightsResult {
    Err(ScmRightsError::UnsupportedPlatform)
}

#[cfg(unix)]
fn send_fd_with_token(
    socket_fd: std::os::fd::RawFd,
    sent_fd: std::os::fd::RawFd,
    token: &[u8; 16],
    socket_path: &Path,
) -> Result<(), ScmRightsError> {
    let mut token_payload = *token;
    let mut iov = libc::iovec {
        iov_base: token_payload.as_mut_ptr().cast(),
        iov_len: token_payload.len(),
    };
    let mut control = vec![0_u8; cmsg_space::<libc::c_int>()];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;

    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(ScmRightsError::SendFailed {
                fd: sent_fd,
                socket: socket_path.to_path_buf(),
                raw_os_error: None,
            });
        }

        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = cmsg_len::<libc::c_int>() as _;
        *libc::CMSG_DATA(header).cast::<libc::c_int>() = sent_fd;
    }

    let sent = unsafe { libc::sendmsg(socket_fd, &message, sendmsg_flags()) };
    if sent < 0 {
        return Err(sendmsg_error(
            sent_fd,
            socket_path,
            std::io::Error::last_os_error(),
        ));
    }
    if sent as usize != token_payload.len() {
        return Err(ScmRightsError::SendFailed {
            fd: sent_fd,
            socket: socket_path.to_path_buf(),
            raw_os_error: None,
        });
    }

    Ok(())
}

#[cfg(unix)]
fn cmsg_space<T>() -> usize {
    unsafe { libc::CMSG_SPACE(std::mem::size_of::<T>() as _) as usize }
}

#[cfg(unix)]
fn cmsg_len<T>() -> usize {
    unsafe { libc::CMSG_LEN(std::mem::size_of::<T>() as _) as usize }
}

#[cfg(all(unix, any(target_os = "android", target_os = "linux")))]
fn sendmsg_flags() -> libc::c_int {
    libc::MSG_NOSIGNAL
}

#[cfg(all(unix, not(any(target_os = "android", target_os = "linux"))))]
fn sendmsg_flags() -> libc::c_int {
    0
}

#[cfg(unix)]
fn socket_connect_error(socket: &Path, error: std::io::Error) -> ScmRightsError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => ScmRightsError::PermissionDenied {
            fd: -1,
            socket: socket.to_path_buf(),
        },
        std::io::ErrorKind::WouldBlock => ScmRightsError::WouldBlock {
            socket: socket.to_path_buf(),
        },
        _ => ScmRightsError::BackendSocketUnavailable {
            socket: socket.to_path_buf(),
        },
    }
}

#[cfg(unix)]
fn sendmsg_error(fd: std::os::fd::RawFd, socket: &Path, error: std::io::Error) -> ScmRightsError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => ScmRightsError::PermissionDenied {
            fd,
            socket: socket.to_path_buf(),
        },
        std::io::ErrorKind::WouldBlock => ScmRightsError::WouldBlock {
            socket: socket.to_path_buf(),
        },
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::NotConnected => ScmRightsError::BackendSocketUnavailable {
            socket: socket.to_path_buf(),
        },
        _ => ScmRightsError::SendFailed {
            fd,
            socket: socket.to_path_buf(),
            raw_os_error: error.raw_os_error(),
        },
    }
}

impl ScmRightsError {
    /// Return the existing attempt-failure classification, when this was a real attempt.
    pub fn attempt_failure(&self) -> Option<HandoffAttemptFailure> {
        match self {
            Self::UnsupportedPlatform => None,
            Self::PermissionDenied { .. } => Some(HandoffAttemptFailure::PermissionDenied),
            Self::BackendSocketUnavailable { .. }
            | Self::WouldBlock { .. }
            | Self::SendFailed { .. }
            | Self::BackendAckTimeout { .. } => Some(HandoffAttemptFailure::BackendAckTimeout),
        }
    }

    /// Map this transport failure into the existing fallback reason vocabulary.
    pub fn fallback_reason(&self) -> HandoffFallbackReason {
        match self.attempt_failure() {
            Some(failure) => failure.into(),
            None => HandoffFallbackReason::ServicePolicyDisabled,
        }
    }

    /// Return the silent reconnect fallback for this transport failure.
    pub fn fallback_decision(&self) -> HandoffFallbackDecision {
        HandoffFallbackDecision::new(self.fallback_reason())
    }

    /// Return the full attempt decision for callers that operate on broker decisions.
    pub fn fallback_attempt_decision(&self) -> HandoffAttemptDecision {
        HandoffAttemptDecision::FallbackToReconnect(self.fallback_decision())
    }

    /// Return true when this error is safe to hide behind reconnect fallback.
    pub fn is_fallback_safe(&self) -> bool {
        let fallback = self.fallback_decision();
        fallback.uses_backend_reconnect() && !fallback.sends_client_error()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::File;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;

    use super::*;

    #[test]
    fn send_scm_rights_to_backend_socket_transfers_fd_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("handoff.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let expected_token = HandoffToken::from_bytes([0x41; 16]);
        let receiver = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            recv_fd_and_token(stream)
        });
        let file = File::open("/dev/null").unwrap();
        let attempt = ScmRightsAttempt::new(
            UnixFileDescriptor::new(file.as_raw_fd()),
            UnixHandoffSocket::new(socket_path),
            expected_token,
        );

        let success = try_send_scm_rights(&attempt).unwrap();
        let (received_fd, received_token) = receiver.join().unwrap();

        assert_eq!(success.sent_fd, attempt.fd);
        assert_eq!(success.handoff_token, expected_token);
        assert_eq!(received_token, expected_token);
        assert_ne!(received_fd, file.as_raw_fd());

        unsafe {
            libc::close(received_fd);
        }
    }

    #[test]
    fn missing_backend_socket_maps_to_fallback_safe_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = UnixHandoffSocket::new(dir.path().join("missing.sock"));
        let file = File::open("/dev/null").unwrap();
        let attempt = ScmRightsAttempt::new(
            UnixFileDescriptor::new(file.as_raw_fd()),
            socket.clone(),
            HandoffToken::from_bytes([0x42; 16]),
        );

        let err = try_send_scm_rights(&attempt).unwrap_err();

        assert!(matches!(
            err,
            ScmRightsError::BackendSocketUnavailable { socket: ref path }
                if path == &socket.path
        ));
        assert!(err.is_fallback_safe());
    }

    fn recv_fd_and_token(stream: UnixStream) -> (RawFd, HandoffToken) {
        let mut token_payload = [0_u8; 16];
        let mut iov = libc::iovec {
            iov_base: token_payload.as_mut_ptr().cast(),
            iov_len: token_payload.len(),
        };
        let mut control = vec![0_u8; cmsg_space::<libc::c_int>()];
        let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;

        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        assert_eq!(received as usize, token_payload.len());

        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        unsafe {
            assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
            let received_fd = *libc::CMSG_DATA(header).cast::<libc::c_int>();
            (received_fd, HandoffToken::from_bytes(token_payload))
        }
    }
}
