//! Unix `SCM_RIGHTS` handoff transport model.
//!
//! This module intentionally does not call `sendmsg` or `recvmsg` yet. It
//! defines the typed request, success, and error surface that the real Unix
//! transport will use, plus the mapping from transport failures into the
//! existing silent reconnect fallback policy.

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
