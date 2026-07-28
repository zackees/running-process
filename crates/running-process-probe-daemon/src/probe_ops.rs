//! `ProbeOps` — the transport-agnostic request core.
//!
//! [`ProbeOps::dispatch`] is a pure function of `(request, peer, conn_id,
//! registry state)`. It performs no I/O.
//!
//! That matters because the daemon has two ingresses — the framed control
//! socket and (later) an HTTP surface. If each owned its own policy logic they
//! would drift, and the weaker one would become the way in. Both call this,
//! so both enforce the same rules and return the same structured errors.
//!
//! It also makes the contract testable without sockets: every state-machine,
//! bounds, replay, and peer-rejection case below is driven by calling
//! `dispatch` directly.

use std::sync::Arc;
use std::time::Duration;

use running_process::broker::server::{PeerCredentialPolicy, PeerIdentity};

use crate::registry::{ProcessKey, RegisterError, RegisterRequest, Registry};
use crate::state::StateError;

/// How often a registrant is expected to heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How long a registration survives without a heartbeat.
///
/// Three missed intervals. This only backstops SIGKILL and similar exits where
/// no connection close ever arrives — a clean disconnect drops the entry
/// immediately and never waits for this.
pub const HEARTBEAT_GRACE: Duration = Duration::from_secs(15);

/// Stable error taxonomy returned by both ingresses.
///
/// Callers branch on these, so the discriminants are part of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeErrorCode {
    /// Request could not be decoded or violated its schema.
    MalformedRequest,
    /// A field exceeded its declared bound.
    OversizeField,
    /// Registration nonce was reused, or the nonce table is full.
    NonceReplay,
    /// Connecting peer is not the daemon's owner.
    PeerRejected,
    /// Operation requires an ARMED registration.
    NotArmed,
    /// Identity verification failed or was not performed.
    IdentityMismatch,
    /// No registration exists for the given key.
    NotRegistered,
}

/// A decoded request, independent of how it arrived.
#[derive(Clone, Debug)]
pub enum ProbeRequest {
    /// Enroll a process.
    Register(Box<RegisterRequest>),
    /// Refresh liveness.
    Heartbeat(ProcessKey),
    /// Voluntarily deregister. Best-effort only — liveness is the real
    /// mechanism, since a crashed process never sends this.
    Unregister(ProcessKey),
}

/// The reply to a [`ProbeRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeReply {
    /// Registration reached ARMED.
    Armed {
        /// The armed identity.
        key: ProcessKey,
    },
    /// Request accepted, nothing further to report.
    Ack,
    /// Request refused, with a stable code and a human-readable reason.
    Refused {
        /// Machine-branchable classification.
        code: ProbeErrorCode,
        /// Human-readable detail. Never parsed by callers.
        reason: String,
    },
}

/// Outcome of verifying a registrant's claimed identity.
///
/// Supplied by the caller rather than computed here: verification touches the
/// filesystem and the process table, and `dispatch` stays I/O-free.
#[derive(Clone, Copy, Debug)]
pub struct IdentityVerdict {
    /// Whether the claimed executable hash, boot id, and liveness all matched.
    pub verified: bool,
    /// Whether the registrant's connection is still open.
    pub connection_alive: bool,
}

/// The daemon's request core.
#[derive(Debug)]
pub struct ProbeOps {
    registry: Arc<Registry>,
    owner_policy: PeerCredentialPolicy,
}

impl ProbeOps {
    /// Build a core over `registry`, accepting only peers `owner_policy` allows.
    pub fn new(registry: Arc<Registry>, owner_policy: PeerCredentialPolicy) -> Self {
        Self {
            registry,
            owner_policy,
        }
    }

    /// The owner policy, for tests that must confirm it agrees with the
    /// registry's owner string.
    #[doc(hidden)]
    pub fn owner_policy_for_test(&self) -> PeerCredentialPolicy {
        self.owner_policy.clone()
    }

    /// Borrow the registry (queries, reaping).
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Handle one request.
    ///
    /// `verdict` carries the caller's identity-verification result and is used
    /// only for `Register`.
    pub fn dispatch(
        &self,
        req: ProbeRequest,
        peer: &PeerIdentity,
        conn_id: u64,
        verdict: IdentityVerdict,
    ) -> ProbeReply {
        // Checked for every request shape, before anything else. The listener
        // already applies this policy, but a second ingress might not, and
        // this is the single place both share.
        if !self.owner_policy.allows(peer) {
            return ProbeReply::Refused {
                code: ProbeErrorCode::PeerRejected,
                reason: format!("peer {} is not the daemon owner", peer.uid_or_sid),
            };
        }

        match req {
            ProbeRequest::Register(request) => self.register(*request, peer, conn_id, verdict),
            ProbeRequest::Heartbeat(key) => match self.registry.heartbeat(&key) {
                Ok(()) => ProbeReply::Ack,
                Err(e) => refuse(e),
            },
            ProbeRequest::Unregister(key) => {
                if let Some(entry) = self.registry.get(&key) {
                    self.registry.drop_by_conn(entry.conn_id);
                    ProbeReply::Ack
                } else {
                    ProbeReply::Refused {
                        code: ProbeErrorCode::NotRegistered,
                        reason: "no registration for this process key".into(),
                    }
                }
            }
        }
    }

    fn register(
        &self,
        request: RegisterRequest,
        peer: &PeerIdentity,
        conn_id: u64,
        verdict: IdentityVerdict,
    ) -> ProbeReply {
        let key = match self.registry.begin_register(request, peer.clone(), conn_id) {
            Ok(k) => k,
            Err(e) => return refuse(e),
        };

        match self
            .registry
            .verify_and_arm(&key, verdict.verified, verdict.connection_alive)
        {
            Ok(()) => ProbeReply::Armed { key },
            Err(e) => refuse(e),
        }
    }
}

/// Map an internal error onto the stable wire taxonomy.
fn refuse(err: RegisterError) -> ProbeReply {
    let code = match &err {
        RegisterError::OversizeField { .. } => ProbeErrorCode::OversizeField,
        RegisterError::NonceReplay => ProbeErrorCode::NonceReplay,
        RegisterError::PeerRejected { .. } => ProbeErrorCode::PeerRejected,
        RegisterError::NotRegistered => ProbeErrorCode::NotRegistered,
        RegisterError::State(StateError::IdentityUnverified) => ProbeErrorCode::IdentityMismatch,
        RegisterError::State(StateError::NoLiveProbe) => ProbeErrorCode::NotArmed,
        RegisterError::State(StateError::Illegal { .. }) => ProbeErrorCode::MalformedRequest,
    };
    ProbeReply::Refused {
        code,
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AllowPolicy, Disclosure};
    use std::path::PathBuf;

    const OWNER: &str = "owner-uid";

    fn ops() -> ProbeOps {
        ProbeOps::new(
            Arc::new(Registry::new(OWNER.into())),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.into(),
            },
        )
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            pid: 1234,
            uid_or_sid: OWNER.into(),
        }
    }

    fn request(pid: u32, nonce: u8) -> Box<RegisterRequest> {
        Box::new(RegisterRequest {
            key: ProcessKey {
                pid,
                started_at_unix_ms: 100,
                boot_id: "boot-1".into(),
            },
            exe_path: PathBuf::from("/usr/bin/app"),
            exe_sha256: [0u8; 32],
            app_class: "clud".into(),
            app_name: "clud".into(),
            app_version: "1.0".into(),
            allow_policy: AllowPolicy::default(),
            disclosure: Disclosure::default(),
            nonce: [nonce; 32],
            supported_ops: vec![],
        })
    }

    fn good() -> IdentityVerdict {
        IdentityVerdict {
            verified: true,
            connection_alive: true,
        }
    }

    #[test]
    fn register_with_verified_identity_arms() {
        let ops = ops();
        let reply = ops.dispatch(ProbeRequest::Register(request(10, 1)), &peer(), 1, good());
        assert!(matches!(reply, ProbeReply::Armed { .. }), "{reply:?}");
    }

    #[test]
    fn register_without_live_probe_is_refused_not_armed() {
        let ops = ops();
        let verdict = IdentityVerdict {
            verified: true,
            connection_alive: false,
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(11, 2)), &peer(), 1, verdict);
        assert_eq!(
            reply,
            ProbeReply::Refused {
                code: ProbeErrorCode::NotArmed,
                reason: "cannot arm: no live probe connection".into(),
            }
        );
    }

    #[test]
    fn register_without_verified_identity_reports_identity_mismatch() {
        let ops = ops();
        let verdict = IdentityVerdict {
            verified: false,
            connection_alive: true,
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(12, 3)), &peer(), 1, verdict);
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::IdentityMismatch),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn foreign_peer_is_refused_before_any_work() {
        let ops = ops();
        let stranger = PeerIdentity {
            pid: 9,
            uid_or_sid: "someone-else".into(),
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(13, 4)), &stranger, 1, good());
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::PeerRejected),
            other => panic!("expected refusal, got {other:?}"),
        }
        assert!(ops.registry().is_empty());
    }

    #[test]
    fn heartbeat_from_a_foreign_peer_is_refused() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(14, 5)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        let stranger = PeerIdentity {
            pid: 9,
            uid_or_sid: "someone-else".into(),
        };
        match ops.dispatch(ProbeRequest::Heartbeat(key), &stranger, 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::PeerRejected),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn replayed_nonce_is_refused_with_its_own_code() {
        let ops = ops();
        ops.dispatch(ProbeRequest::Register(request(15, 6)), &peer(), 1, good());
        let reply = ops.dispatch(ProbeRequest::Register(request(16, 6)), &peer(), 2, good());
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::NonceReplay),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn oversize_field_is_refused_with_its_own_code() {
        let ops = ops();
        let mut req = request(17, 7);
        req.app_name = "x".repeat(1024);
        match ops.dispatch(ProbeRequest::Register(req), &peer(), 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::OversizeField),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_after_unregister_is_refused() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(18, 8)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        assert_eq!(
            ops.dispatch(ProbeRequest::Unregister(key.clone()), &peer(), 1, good()),
            ProbeReply::Ack
        );
        match ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::NotRegistered),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_on_a_live_registration_is_acked() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(19, 9)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        assert_eq!(
            ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 1, good()),
            ProbeReply::Ack
        );
    }

    /// The grace window only exists for exits that send no close.
    #[test]
    fn grace_is_three_missed_intervals() {
        assert_eq!(HEARTBEAT_GRACE, HEARTBEAT_INTERVAL * 3);
    }
}
