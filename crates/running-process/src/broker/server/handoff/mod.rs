//! Handoff scaffolding for the optional Phase 6 handle-passing path.
//!
//! Platform-specific handle transfer is intentionally not wired here. This
//! module owns the reusable verification pieces that both `DuplicateHandle`
//! and `SCM_RIGHTS` paths will need before a handed-off connection is trusted.

pub mod handoff_token;

pub use handoff_token::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig,
    DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS, DEFAULT_HANDOFF_TOKEN_TTL,
    DEFAULT_MAX_PENDING_HANDOFF_TOKENS, HANDOFF_TOKEN_BYTES,
};
