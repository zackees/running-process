//! Handoff scaffolding for the optional Phase 6 handle-passing path.
//!
//! Platform-specific handle transfer is intentionally not wired here. This
//! module owns the reusable verification pieces that both `DuplicateHandle`
//! and `SCM_RIGHTS` paths will need before a handed-off connection is trusted.

pub mod fallback;
pub mod handoff_token;
pub mod unix;
pub mod windows;

pub use fallback::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffAttemptInputs, HandoffFallbackDecision,
    HandoffFallbackPolicy, HandoffFallbackReason, HandoffFallbackState,
    DEFAULT_HANDOFF_FAILED_ATTEMPTS_PER_WINDOW, DEFAULT_HANDOFF_FAILED_ATTEMPT_WINDOW,
};
pub use handoff_token::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig,
    DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS, DEFAULT_HANDOFF_TOKEN_TTL,
    DEFAULT_MAX_PENDING_HANDOFF_TOKENS, HANDOFF_TOKEN_BYTES,
};
pub use unix::{
    ScmRightsAttempt, ScmRightsError, ScmRightsResult, ScmRightsSuccess, UnixFileDescriptor,
    UnixHandoffSocket, SCM_RIGHTS_TRANSPORT_SUPPORTED,
};
pub use windows::{
    DuplicateHandleAttempt, DuplicateHandleError, DuplicateHandleResult, DuplicateHandleSuccess,
    WindowsHandleValue, DUPLICATE_HANDLE_TRANSPORT_SUPPORTED,
};
