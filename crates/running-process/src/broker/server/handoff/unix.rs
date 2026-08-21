//! Unix `SCM_RIGHTS` handoff transport model.
//!
//! This module preserves the public 4.x transport model and maps the selected
//! platform IPC primitive into the existing silent reconnect fallback policy.
//! Native `sendmsg(SCM_RIGHTS)` mechanics live in the platform package.

use std::path::PathBuf;

use super::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason,
    HandoffToken,
};

/// Whether this build target can eventually use Unix-domain `SCM_RIGHTS`.
pub const SCM_RIGHTS_TRANSPORT_SUPPORTED: bool =
    running_process_platform_internal::LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED;

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
    running_process_platform_internal::legacy_send_fd_to(
        &attempt.backend_socket.path,
        attempt.fd.raw(),
        attempt.handoff_token.as_bytes(),
    )
    .map_err(|error| legacy_error(attempt, error, true))?;
    Ok(ScmRightsSuccess::new(
        attempt.fd,
        attempt.backend_socket.clone(),
        attempt.handoff_token,
    ))
}

/// Send the broker-held file descriptor and token over an already-connected
/// Unix-domain handoff socket.
///
/// [`try_send_scm_rights`] dials a fresh connection per attempt; the
/// production serve path instead reuses the framed broker↔backend handoff
/// connection so the `SCM_RIGHTS` message and the [`HandoffOffer`
/// frame](crate::broker::protocol::HandoffOffer) travel over the same
/// stream. The caller keeps ownership of both descriptors.
pub fn try_send_scm_rights_over(socket_fd: i32, attempt: &ScmRightsAttempt) -> ScmRightsResult {
    running_process_platform_internal::legacy_send_fd_over(
        socket_fd,
        attempt.fd.raw(),
        attempt.handoff_token.as_bytes(),
    )
    .map_err(|error| legacy_error(attempt, error, false))?;
    Ok(ScmRightsSuccess::new(
        attempt.fd,
        attempt.backend_socket.clone(),
        attempt.handoff_token,
    ))
}

fn legacy_error(
    attempt: &ScmRightsAttempt,
    error: running_process_platform_internal::LegacyHandoffError,
    connecting: bool,
) -> ScmRightsError {
    use running_process_platform_internal::platform::ipc::HandoffTransferErrorKind;

    if let Some((sent_bytes, expected_bytes)) = error.partial_counts() {
        return ScmRightsError::PartialSend {
            fd: attempt.fd.raw(),
            socket: attempt.backend_socket.path.clone(),
            sent_bytes,
            expected_bytes,
        };
    }
    match error.kind() {
        HandoffTransferErrorKind::Unsupported => ScmRightsError::UnsupportedPlatform,
        HandoffTransferErrorKind::PermissionDenied => ScmRightsError::PermissionDenied {
            fd: if connecting { -1 } else { attempt.fd.raw() },
            socket: attempt.backend_socket.path.clone(),
        },
        HandoffTransferErrorKind::BackendUnavailable => ScmRightsError::BackendSocketUnavailable {
            socket: attempt.backend_socket.path.clone(),
        },
        HandoffTransferErrorKind::WouldBlock => ScmRightsError::WouldBlock {
            socket: attempt.backend_socket.path.clone(),
        },
        HandoffTransferErrorKind::Failed => ScmRightsError::SendFailed {
            fd: attempt.fd.raw(),
            socket: attempt.backend_socket.path.clone(),
            raw_os_error: error.raw_os_error(),
        },
    }
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
    /// Descriptor transfer succeeded but the follow-up protocol offer failed.
    #[error("handoff offer delivery failed after passing fd {fd} to backend socket {socket}")]
    PostTransferDeliveryFailed {
        /// File descriptor targeted by the handoff.
        fd: i32,
        /// Backend handoff socket path.
        socket: PathBuf,
    },
    /// Some token bytes were sent, so the descriptor may have reached the backend.
    #[error(
        "SCM_RIGHTS send was partial ({sent_bytes}/{expected_bytes} bytes) for fd {fd} to backend handoff socket {socket}"
    )]
    PartialSend {
        /// File descriptor targeted by the handoff.
        fd: i32,
        /// Backend handoff socket path.
        socket: PathBuf,
        /// Token bytes accepted by the socket.
        sent_bytes: usize,
        /// Complete token length required by the protocol.
        expected_bytes: usize,
    },
    /// The backend did not acknowledge the passed file descriptor before the deadline.
    #[error("backend handoff socket {socket} did not acknowledge passed fd")]
    BackendAckTimeout {
        /// Backend handoff socket path.
        socket: PathBuf,
    },
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
            | Self::PostTransferDeliveryFailed { .. }
            | Self::PartialSend { .. }
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

    /// Return true when the backend may already own the duplicated descriptor.
    ///
    /// Stream sockets attach `SCM_RIGHTS` to the first delivered byte, so a
    /// positive short send is indeterminate even though the complete token was
    /// not delivered. The orchestrator revokes that token before fallback.
    pub fn fd_may_have_reached_backend(&self) -> bool {
        matches!(self, Self::PostTransferDeliveryFailed { .. })
            || matches!(self, Self::PartialSend { sent_bytes, .. } if *sent_bytes > 0)
    }
}

#[cfg(test)]
mod platform_neutral_tests {
    use super::ScmRightsError;

    #[test]
    fn positive_partial_send_tracks_indeterminate_fd_delivery() {
        let error = ScmRightsError::PartialSend {
            fd: 7,
            socket: "handoff".into(),
            sent_bytes: 1,
            expected_bytes: 16,
        };

        assert!(error.fd_may_have_reached_backend());
        assert!(error.is_fallback_safe());
    }

    #[test]
    fn failed_offer_after_transfer_tracks_backend_ownership() {
        let error = ScmRightsError::PostTransferDeliveryFailed {
            fd: 7,
            socket: "handoff".into(),
        };

        assert!(error.fd_may_have_reached_backend());
        assert!(error.is_fallback_safe());
    }
}
