//! Composite broker/daemon session token authority (zackees/soldr#2360, #2361
//! Phase 1, #2363).
//!
//! **STATUS: wired into [`super::hello_handler::HelloHandler`] via
//! `with_session_token_authority` — opt-in, dormant unless a caller
//! configures it. See "Not done yet" below for what's still open.**
//!
//! ## What this is — a cooperative invalidation signal, NOT authentication
//!
//! This is a liveness/generation notification scheme for a cooperative
//! client inside one trust domain (all roles run as the same user on the
//! same machine). It is **not** a security boundary and does not
//! authenticate anyone.
//!
//! Every client session carries a composite token `broker_token ‖
//! daemon_token`: the first half minted once by the broker at its own
//! startup, the second minted by the specific daemon the client is talking
//! to. The halves are generation markers — "the broker/daemon incarnation
//! you established this session against". When a presented token stops
//! validating, that tells the client its session has terminated or the
//! broker/daemon got forcefully cycled since its last message: the session
//! is invalid, and the client should report the error (the
//! cancelled-because-stopped message class, soldr#2363), unwind, and exit 1
//! — never retry against the new incarnation as if nothing happened.
//! Two-level invalidation falls out of the split for free:
//!
//! - Rotating the **broker** half signals every session across every
//!   daemon at once (broker restart, or a live rotation e.g. after the
//!   spawn-storm guard trips).
//! - Invalidating one **daemon**'s half signals only that daemon's
//!   sessions; sessions against other daemons are unaffected.
//!
//! The halves come from OS randomness only so that incarnations are
//! globally unique — a restarted broker or daemon can never accidentally
//! validate a stale token minted by its predecessor, the way a counter or
//! timestamp could collide. The bytes are not secrets guarding anything,
//! which is also why the plain `!=` comparison (mirroring
//! [`super::handoff::HandoffToken`]) is fine here: constant-time comparison
//! defends secrets against guessing oracles, and there is no secret and
//! nothing to guess for.
//!
//! There is deliberately **no TTL** on these tokens. A session may go
//! silent for arbitrarily long (e.g. a link phase with no daemon traffic)
//! and remain valid; invalidation is communicated lazily, just in time, on
//! the next communication intent — the client learns its session died at
//! the exact moment it next tries to use it, which is the only moment it
//! matters.
//!
//! This module is the authority that mints, rotates, and validates both
//! halves. It deliberately knows nothing about the wire (`Hello.auth_token`,
//! already reserved on the v2-reuses-v1-framing Hello message — see
//! `broker_v1_envelope.proto`) or about `HelloHandler` /
//! `RegisteredBackend` — see "Not done yet".
//!
//! ## Not done yet (left for a follow-up slice)
//!
//! - `daemon_id` is resolved as `hello.service_name` — the same key
//!   `RegisteredBackend` already uses — rather than a new field. `Refused`
//!   uses `ERROR_PEER_REJECTED` for every [`SessionTokenRejection`] kind.
//!   Both settled in the `HelloHandler` wiring.
//! - Where the authority instance itself lives (per-broker-process
//!   singleton state) and how `register_daemon`/`invalidate_daemon` are
//!   threaded into the daemon spawn/exit lifecycle — that's soldr#2361
//!   Phase 2 (spawn-chain inversion), which is what will actually call
//!   `with_session_token_authority` and stop this from being dormant.
//! - Persistence-boundary invariant from soldr#2363's testing invariants
//!   ("no token material is ever written under a daemon cache root") —
//!   this module is pure in-memory today, so that invariant holds trivially
//!   for it, but the caller that eventually persists broker-side session
//!   state must uphold it too.

use std::collections::HashMap;
use std::fmt;

/// Number of bytes in one half of the composite token (128 bits), matching
/// [`super::handoff::HandoffToken`]'s existing size for consistency.
pub const SESSION_TOKEN_HALF_BYTES: usize = 16;

/// Total presented-token length: broker half + daemon half.
pub const SESSION_TOKEN_TOTAL_BYTES: usize = SESSION_TOKEN_HALF_BYTES * 2;

/// One 128-bit half of a composite session token. Used for both the
/// broker-minted half and each daemon-minted half — the two are typed
/// identically; which is which is a matter of which map an instance is
/// looked up in, not a type-level distinction.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHalf([u8; SESSION_TOKEN_HALF_BYTES]);

impl TokenHalf {
    /// Mint one half from operating-system randomness.
    pub fn generate() -> Result<Self, SessionTokenError> {
        let mut bytes = [0_u8; SESSION_TOKEN_HALF_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Build a half from exact bytes (tests; wire decode).
    pub fn from_bytes(bytes: [u8; SESSION_TOKEN_HALF_BYTES]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes, e.g. to concatenate into a presented token.
    pub fn as_bytes(&self) -> &[u8; SESSION_TOKEN_HALF_BYTES] {
        &self.0
    }
}

impl fmt::Debug for TokenHalf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHalf(<redacted>)")
    }
}

/// Opaque identifier for one daemon's token slot. Deliberately a plain
/// `String` rather than reusing `ServiceDefinition`'s type, since how a
/// daemon identity maps to a registered backend is one of the open
/// wiring questions above — this keeps the authority decoupled from that
/// decision until it's made.
pub type DaemonId = String;

/// Why a presented composite token failed validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTokenRejection {
    /// Presented bytes were not exactly [`SESSION_TOKEN_TOTAL_BYTES`] long.
    MalformedLength,
    /// The first half did not match the broker's current token — this is
    /// the broker-rotation invalidation path: EVERY client sees this
    /// after a rotation, regardless of which daemon they were talking to.
    BrokerHalfMismatch,
    /// The broker half matched, but the daemon named by `daemon_id` has no
    /// registered token (never registered, or already invalidated).
    DaemonUnknown,
    /// Both halves resolved to real tokens, but the daemon half did not
    /// match — this is the single-daemon invalidation path: only sessions
    /// naming this `daemon_id` see this.
    DaemonHalfMismatch,
}

/// Mints, rotates, and validates the composite `broker_token ‖ daemon_token`
/// pair described in the module docs.
#[derive(Debug)]
pub struct SessionTokenAuthority {
    broker_token: TokenHalf,
    daemon_tokens: HashMap<DaemonId, TokenHalf>,
}

impl SessionTokenAuthority {
    /// Mint a fresh broker token from OS randomness. Call once at broker
    /// startup.
    pub fn new() -> Result<Self, SessionTokenError> {
        Ok(Self {
            broker_token: TokenHalf::generate()?,
            daemon_tokens: HashMap::new(),
        })
    }

    /// Test/deterministic constructor — production code should use
    /// [`Self::new`] so the broker half comes from real randomness.
    pub fn with_broker_token(broker_token: TokenHalf) -> Self {
        Self {
            broker_token,
            daemon_tokens: HashMap::new(),
        }
    }

    /// The current broker-half bytes, for a caller (e.g. the front door
    /// spawning this broker) to hand to a newly-connecting client alongside
    /// the daemon half.
    pub fn broker_token(&self) -> &TokenHalf {
        &self.broker_token
    }

    /// Rotate the broker token in place — the live-rotation path (e.g. the
    /// spawn-storm guard tripping). Every previously-issued composite token
    /// stops validating immediately: [`Self::validate`] compares against
    /// the NEW value from the moment this returns.
    pub fn rotate_broker_token(&mut self) -> Result<TokenHalf, SessionTokenError> {
        let fresh = TokenHalf::generate()?;
        self.broker_token = fresh;
        Ok(fresh)
    }

    /// Register (or re-register) a daemon's token, minted fresh from OS
    /// randomness. Call once at that daemon's startup.
    pub fn register_daemon(&mut self, daemon_id: DaemonId) -> Result<TokenHalf, SessionTokenError> {
        let token = TokenHalf::generate()?;
        self.daemon_tokens.insert(daemon_id, token);
        Ok(token)
    }

    /// Remove a daemon's token entirely — every session naming this
    /// `daemon_id` fails [`Self::validate`] with [`SessionTokenRejection::DaemonUnknown`]
    /// from this call onward. Sessions naming any other `daemon_id` are
    /// unaffected. Returns whether a token was actually present.
    pub fn invalidate_daemon(&mut self, daemon_id: &str) -> bool {
        self.daemon_tokens.remove(daemon_id).is_some()
    }

    /// How many daemons currently hold a registered token.
    pub fn daemon_count(&self) -> usize {
        self.daemon_tokens.len()
    }

    /// Validate a presented composite token against a claimed `daemon_id`
    /// (which daemon the client's Hello says it wants — see "Not done yet"
    /// for how that claim reaches here from the wire).
    ///
    /// `presented` is `broker_half ‖ daemon_half`, [`SESSION_TOKEN_TOTAL_BYTES`]
    /// long. Checks the broker half FIRST and independently of daemon
    /// lookup, so a broker cycle reports as
    /// [`SessionTokenRejection::BrokerHalfMismatch`] even when the named
    /// `daemon_id` is also gone — the broker-wide event is the more global
    /// (and more actionable) verdict, and per-daemon state after a broker
    /// cycle is stale by definition, so reporting it would mislead the
    /// client about what happened.
    pub fn validate(&self, presented: &[u8], daemon_id: &str) -> Result<(), SessionTokenRejection> {
        if presented.len() != SESSION_TOKEN_TOTAL_BYTES {
            return Err(SessionTokenRejection::MalformedLength);
        }
        let (broker_half, daemon_half) = presented.split_at(SESSION_TOKEN_HALF_BYTES);

        if broker_half != self.broker_token.as_bytes() {
            return Err(SessionTokenRejection::BrokerHalfMismatch);
        }

        let Some(expected_daemon_token) = self.daemon_tokens.get(daemon_id) else {
            return Err(SessionTokenRejection::DaemonUnknown);
        };
        if daemon_half != expected_daemon_token.as_bytes() {
            return Err(SessionTokenRejection::DaemonHalfMismatch);
        }

        Ok(())
    }
}

/// Concatenate a broker half and a daemon half into one presented-token
/// byte vector, matching what [`SessionTokenAuthority::validate`] expects.
/// A convenience for a client-side caller assembling `Hello.auth_token`.
pub fn compose_presented_token(broker_half: &TokenHalf, daemon_half: &TokenHalf) -> Vec<u8> {
    let mut out = Vec::with_capacity(SESSION_TOKEN_TOTAL_BYTES);
    out.extend_from_slice(broker_half.as_bytes());
    out.extend_from_slice(daemon_half.as_bytes());
    out
}

/// Errors raised while minting session-token halves.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionTokenError {
    /// Random byte generation failed.
    #[error("session token random generation failed: {0}")]
    Random(String),
}

impl From<getrandom::Error> for SessionTokenError {
    fn from(value: getrandom::Error) -> Self {
        Self::Random(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half(byte: u8) -> TokenHalf {
        TokenHalf::from_bytes([byte; SESSION_TOKEN_HALF_BYTES])
    }

    #[test]
    fn generate_produces_distinct_halves() {
        let a = TokenHalf::generate().expect("random");
        let b = TokenHalf::generate().expect("random");
        assert_ne!(a.as_bytes(), b.as_bytes(), "two mints must not collide");
    }

    #[test]
    fn valid_composite_token_validates() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let daemon_token = authority.register_daemon("daemon-1".into()).expect("mint");
        let presented = compose_presented_token(authority.broker_token(), &daemon_token);

        assert_eq!(authority.validate(&presented, "daemon-1"), Ok(()));
    }

    #[test]
    fn wrong_broker_half_is_rejected_even_for_a_valid_daemon() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let daemon_token = authority.register_daemon("daemon-1".into()).expect("mint");
        let wrong_broker_half = half(0xFF);
        let presented = compose_presented_token(&wrong_broker_half, &daemon_token);

        assert_eq!(
            authority.validate(&presented, "daemon-1"),
            Err(SessionTokenRejection::BrokerHalfMismatch)
        );
    }

    #[test]
    fn broker_half_is_checked_before_daemon_lookup_for_an_unknown_daemon() {
        // A wrong broker half against a daemon_id that was never
        // registered must still report BrokerHalfMismatch, not
        // DaemonUnknown -- after a broker cycle the broker-wide verdict
        // is the one the client must act on; per-daemon state is stale
        // by definition and would misattribute what happened.
        let authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let wrong_broker_half = half(0xFF);
        let daemon_half = half(0x11);
        let presented = compose_presented_token(&wrong_broker_half, &daemon_half);

        assert_eq!(
            authority.validate(&presented, "never-registered"),
            Err(SessionTokenRejection::BrokerHalfMismatch)
        );
    }

    #[test]
    fn correct_broker_half_but_unregistered_daemon_is_rejected() {
        let authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let daemon_half = half(0x11);
        let presented = compose_presented_token(&half(0xAA), &daemon_half);

        assert_eq!(
            authority.validate(&presented, "never-registered"),
            Err(SessionTokenRejection::DaemonUnknown)
        );
    }

    #[test]
    fn wrong_daemon_half_is_rejected() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        authority.register_daemon("daemon-1".into()).expect("mint");
        let wrong_daemon_half = half(0xEE);
        let presented = compose_presented_token(&half(0xAA), &wrong_daemon_half);

        assert_eq!(
            authority.validate(&presented, "daemon-1"),
            Err(SessionTokenRejection::DaemonHalfMismatch)
        );
    }

    #[test]
    fn malformed_length_is_rejected_before_any_comparison() {
        let authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        assert_eq!(
            authority.validate(&[0xAA; 5], "daemon-1"),
            Err(SessionTokenRejection::MalformedLength)
        );
        assert_eq!(
            authority.validate(&[], "daemon-1"),
            Err(SessionTokenRejection::MalformedLength)
        );
        assert_eq!(
            authority.validate(&[0xAA; SESSION_TOKEN_TOTAL_BYTES + 1], "daemon-1"),
            Err(SessionTokenRejection::MalformedLength)
        );
    }

    #[test]
    fn broker_rotation_invalidates_every_daemons_sessions() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let daemon_a = authority.register_daemon("daemon-a".into()).expect("mint");
        let daemon_b = authority.register_daemon("daemon-b".into()).expect("mint");
        let old_broker_half = *authority.broker_token();

        let presented_a = compose_presented_token(&old_broker_half, &daemon_a);
        let presented_b = compose_presented_token(&old_broker_half, &daemon_b);
        assert_eq!(authority.validate(&presented_a, "daemon-a"), Ok(()));
        assert_eq!(authority.validate(&presented_b, "daemon-b"), Ok(()));

        authority.rotate_broker_token().expect("rotate");

        // Same presented bytes as before -- both daemons' sessions are now
        // invalid, proving rotation is a broker-wide invalidation, not
        // scoped to one daemon.
        assert_eq!(
            authority.validate(&presented_a, "daemon-a"),
            Err(SessionTokenRejection::BrokerHalfMismatch)
        );
        assert_eq!(
            authority.validate(&presented_b, "daemon-b"),
            Err(SessionTokenRejection::BrokerHalfMismatch)
        );
    }

    #[test]
    fn invalidating_one_daemon_does_not_disrupt_another() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let daemon_a = authority.register_daemon("daemon-a".into()).expect("mint");
        let daemon_b = authority.register_daemon("daemon-b".into()).expect("mint");
        let broker_half = *authority.broker_token();

        let presented_a = compose_presented_token(&broker_half, &daemon_a);
        let presented_b = compose_presented_token(&broker_half, &daemon_b);

        assert!(authority.invalidate_daemon("daemon-a"));

        assert_eq!(
            authority.validate(&presented_a, "daemon-a"),
            Err(SessionTokenRejection::DaemonUnknown),
            "daemon-a's session must be gone"
        );
        assert_eq!(
            authority.validate(&presented_b, "daemon-b"),
            Ok(()),
            "daemon-b's session must be completely unaffected"
        );
    }

    #[test]
    fn invalidate_daemon_reports_whether_a_token_was_present() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        assert!(!authority.invalidate_daemon("never-registered"));

        authority.register_daemon("daemon-1".into()).expect("mint");
        assert!(authority.invalidate_daemon("daemon-1"));
        // Second call: already gone.
        assert!(!authority.invalidate_daemon("daemon-1"));
    }

    #[test]
    fn re_registering_a_daemon_mints_a_new_token_and_invalidates_the_old_one() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        let broker_half = *authority.broker_token();
        let first_token = authority.register_daemon("daemon-1".into()).expect("mint");
        let stale_presented = compose_presented_token(&broker_half, &first_token);

        // Simulates a daemon restarting under the same daemon_id (e.g. the
        // #2352 version-thrash scenario) and re-registering.
        let second_token = authority.register_daemon("daemon-1".into()).expect("mint");
        assert_ne!(first_token.as_bytes(), second_token.as_bytes());

        assert_eq!(
            authority.validate(&stale_presented, "daemon-1"),
            Err(SessionTokenRejection::DaemonHalfMismatch),
            "the pre-restart token must no longer validate"
        );
    }

    #[test]
    fn daemon_count_tracks_registration_and_invalidation() {
        let mut authority = SessionTokenAuthority::with_broker_token(half(0xAA));
        assert_eq!(authority.daemon_count(), 0);
        authority.register_daemon("daemon-1".into()).expect("mint");
        authority.register_daemon("daemon-2".into()).expect("mint");
        assert_eq!(authority.daemon_count(), 2);
        authority.invalidate_daemon("daemon-1");
        assert_eq!(authority.daemon_count(), 1);
    }

    #[test]
    fn debug_impl_redacts_token_bytes() {
        let token = TokenHalf::from_bytes([0x42; SESSION_TOKEN_HALF_BYTES]);
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains("42"),
            "token bytes must not leak into Debug output: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }
}
