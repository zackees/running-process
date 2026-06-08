//! Windows `DuplicateHandle` handoff transport model.
//!
//! This module intentionally does not call `OpenProcess` or
//! `DuplicateHandle` yet. It defines the typed request, success, and error
//! surface that the real Windows transport will use, plus the mapping from
//! transport failures into the existing silent reconnect fallback policy.

use super::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason,
    HandoffToken,
};

/// Whether this build target can eventually use the Windows handoff transport.
pub const DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool = cfg!(windows);

/// Opaque raw Windows handle value held by the broker or duplicated into a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowsHandleValue(usize);

impl WindowsHandleValue {
    /// Build an opaque handle value for transport bookkeeping.
    pub fn new(value: usize) -> Self {
        Self(value)
    }

    /// Return the raw opaque handle value.
    pub fn get(self) -> usize {
        self.0
    }
}

/// Inputs for one future `DuplicateHandle` attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateHandleAttempt {
    /// Broker-owned pipe handle to duplicate.
    pub pipe_handle: WindowsHandleValue,
    /// Backend process ID that should receive the duplicated handle.
    pub backend_pid: u32,
    /// One-time token associated with this handoff attempt.
    pub handoff_token: HandoffToken,
}

impl DuplicateHandleAttempt {
    /// Build typed inputs for one `DuplicateHandle` attempt.
    pub fn new(
        pipe_handle: WindowsHandleValue,
        backend_pid: u32,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            pipe_handle,
            backend_pid,
            handoff_token,
        }
    }
}

/// Successful `DuplicateHandle` outcome once real handle passing is wired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateHandleSuccess {
    /// Handle value duplicated into the backend process.
    pub duplicated_handle: WindowsHandleValue,
    /// Backend process ID that received the duplicated handle.
    pub backend_pid: u32,
    /// One-time token paired with the duplicated handle.
    pub handoff_token: HandoffToken,
}

impl DuplicateHandleSuccess {
    /// Build a typed successful handoff result.
    pub fn new(
        duplicated_handle: WindowsHandleValue,
        backend_pid: u32,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            duplicated_handle,
            backend_pid,
            handoff_token,
        }
    }
}

/// Result returned by the future Windows transport.
pub type DuplicateHandleResult = Result<DuplicateHandleSuccess, DuplicateHandleError>;

/// Failure from a future `DuplicateHandle` handoff attempt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DuplicateHandleError {
    /// The current target cannot use the Windows handoff transport.
    #[error("DuplicateHandle handoff transport is unsupported on this platform")]
    UnsupportedPlatform,
    /// Opening the backend process for `PROCESS_DUP_HANDLE` failed.
    #[error("cannot open backend process {backend_pid} for DuplicateHandle")]
    CannotOpenBackend {
        /// Backend process ID that could not be opened.
        backend_pid: u32,
    },
    /// The platform denied handle duplication.
    #[error("permission denied duplicating handle into backend process {backend_pid}")]
    PermissionDenied {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
    /// The broker and backend trust or integrity levels are incompatible.
    #[error("integrity mismatch duplicating handle into backend process {backend_pid}")]
    IntegrityMismatch {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
    /// The backend did not acknowledge the duplicated handle before the deadline.
    #[error("backend process {backend_pid} did not acknowledge duplicated handle")]
    BackendAckTimeout {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
}

impl DuplicateHandleError {
    /// Return the existing attempt-failure classification, when this was a real attempt.
    pub fn attempt_failure(&self) -> Option<HandoffAttemptFailure> {
        match self {
            Self::UnsupportedPlatform => None,
            Self::CannotOpenBackend { .. } | Self::PermissionDenied { .. } => {
                Some(HandoffAttemptFailure::PermissionDenied)
            }
            Self::IntegrityMismatch { .. } => Some(HandoffAttemptFailure::IntegrityMismatch),
            Self::BackendAckTimeout { .. } => Some(HandoffAttemptFailure::BackendAckTimeout),
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
