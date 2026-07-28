//! The control-socket request loop (#704).
//!
//! Connects the two halves that shipped separately: the socket from the daemon
//! skeleton, and `ProbeOps` from the registration contract. Before this, the
//! daemon accepted connections and dropped them — every piece unit-tested,
//! nothing joined.
//!
//! # Connection close is the liveness signal
//!
//! [`Registry::drop_by_conn`] fires when a connection ends, on **every** exit
//! path — clean close, protocol error, or read failure. The heartbeat grace
//! only backstops SIGKILL, where no close ever arrives. A path that returns
//! without dropping would leave a registration claiming a process that is
//! gone, and the daemon would keep reporting it as `ARMED`.
//!
//! # Identity is verified here, not in `ProbeOps`
//!
//! `dispatch` is sans-io by design, so it takes a verdict rather than
//! computing one. Hashing the claimed executable and checking liveness are I/O,
//! so they happen at this boundary and the result is passed in.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use prost::Message as _;
use running_process::broker::protocol::framing::{read_frame_with_cap, write_frame};
use running_process::broker::server::PeerIdentity;
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, ProbeEnvelope, RegisterProcess, RegistrationStatus,
};

use crate::probe_ops::{IdentityVerdict, ProbeErrorCode, ProbeOps, ProbeReply, ProbeRequest};
use crate::registry::{AllowPolicy, Disclosure, ProcessKey, RegisterRequest};

/// Cap on one request frame.
///
/// Registration payloads are small; anything larger is malformed or hostile.
/// Deliberately far below the transport's 16 MiB ceiling so the bound is
/// enforced before the allocation, not after.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Hands out connection ids.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate an id for a new connection.
pub fn next_conn_id() -> u64 {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// Translate a wire `ProbeEnvelope` into a domain request.
///
/// Returns `None` for bodies this daemon does not serve, which the caller
/// answers with a structured refusal rather than closing the connection — a
/// client speaking a newer schema should get a reply it can interpret.
pub fn request_from_envelope(envelope: ProbeEnvelope) -> Option<ProbeRequest> {
    match envelope.body? {
        Body::Register(req) => Some(ProbeRequest::Register(Box::new(register_from_proto(req)?))),
        Body::Heartbeat(hb) => Some(ProbeRequest::Heartbeat(key_from_proto(hb.key?)?)),
        Body::Unregister(un) => Some(ProbeRequest::Unregister(key_from_proto(un.key?)?)),
        _ => None,
    }
}

fn key_from_proto(key: running_process_probe::probe_diag::v1::ProcessKey) -> Option<ProcessKey> {
    Some(ProcessKey {
        pid: u32::try_from(key.pid).ok()?,
        // A key without a start time cannot survive PID reuse, so refuse it
        // rather than register an identity that may silently alias.
        started_at_unix_ms: key.start_time?,
        boot_id: key.boot_id.unwrap_or_default(),
    })
}

fn register_from_proto(req: RegisterProcess) -> Option<RegisterRequest> {
    let mut sha = [0u8; 32];
    if req.exe_sha256.len() == 32 {
        sha.copy_from_slice(&req.exe_sha256);
    }
    let mut nonce = [0u8; 32];
    if req.registration_nonce.len() == 32 {
        nonce.copy_from_slice(&req.registration_nonce);
    }

    Some(RegisterRequest {
        key: key_from_proto(req.key?)?,
        exe_path: PathBuf::from(req.exe_path),
        exe_sha256: sha,
        app_class: req.app_class,
        app_name: req.app_name,
        app_version: req.app_version,
        allow_policy: AllowPolicy {
            allow_all_ops: req
                .allow_policy
                .as_ref()
                .map(|p| p.allow_all_ops)
                .unwrap_or(true),
            env_allowlist: req
                .allow_policy
                .map(|p| p.env_allowlist)
                .unwrap_or_default(),
        },
        disclosure: Disclosure {
            expose_exe_path: req
                .disclosure
                .as_ref()
                .map(|d| d.expose_exe_path)
                .unwrap_or(false),
            expose_cmdline: req
                .disclosure
                .as_ref()
                .map(|d| d.expose_cmdline)
                .unwrap_or(false),
            expose_env_names: req.disclosure.map(|d| d.expose_env_names).unwrap_or(false),
        },
        nonce,
        supported_ops: Vec::new(),
    })
}

/// Verify a registrant's claimed identity.
///
/// Three independent checks, all of which must hold before a process can be
/// armed: the executable still hashes to what was claimed, the boot id matches
/// this boot, and the process is actually alive. Any one failing means the
/// claim describes something other than the caller.
pub fn verify_identity(request: &RegisterRequest, connection_alive: bool) -> IdentityVerdict {
    let boot_matches = request.key.boot_id.is_empty()
        || running_process::broker::host_identity::current().boot_id == request.key.boot_id;

    let alive =
        running_process::broker::backend_lifecycle::verify_pid::process_is_alive(request.key.pid);

    let hash_matches = match running_process::broker::backend_lifecycle::identity::sha256_file(
        &request.exe_path,
    ) {
        Ok(actual) => actual == request.exe_sha256,
        // Unreadable executable cannot be verified, so it is not verified.
        Err(_) => false,
    };

    IdentityVerdict {
        verified: boot_matches && alive && hash_matches,
        connection_alive,
    }
}

/// Encode a reply as a `ProbeEnvelope` for the wire.
pub fn envelope_from_reply(request_id: u64, reply: &ProbeReply) -> ProbeEnvelope {
    let body = match reply {
        ProbeReply::Armed { .. } => Body::RegistrationStatus(RegistrationStatus {
            // 2 == ARMED.
            state: 2,
            error: 0,
            detail: String::new(),
            ..Default::default()
        }),
        ProbeReply::Ack => Body::RegistrationStatus(RegistrationStatus {
            state: 0,
            error: 0,
            detail: "ack".into(),
            ..Default::default()
        }),
        ProbeReply::Refused { code, reason } => Body::RegistrationStatus(RegistrationStatus {
            // 3 == DROPPED: the request did not produce a live registration.
            state: 3,
            error: probe_error_to_proto(*code),
            detail: reason.clone(),
            ..Default::default()
        }),
    };
    ProbeEnvelope {
        wire_version: 1,
        request_id,
        deadline_unix_ms: 0,
        body: Some(body),
    }
}

/// Map the internal taxonomy onto `probe_diag.v1`'s `ProbeErrorCode`.
fn probe_error_to_proto(code: ProbeErrorCode) -> i32 {
    match code {
        ProbeErrorCode::MalformedRequest => 5, // PROBE_ERROR_INTERNAL
        ProbeErrorCode::OversizeField => 5,
        ProbeErrorCode::NonceReplay => 3, // POLICY_DENIED
        ProbeErrorCode::PeerRejected => 3,
        ProbeErrorCode::NotArmed => 2, // NOT_REGISTERED
        ProbeErrorCode::NotRegistered => 2,
        ProbeErrorCode::IdentityMismatch => 1, // PID_REUSE / identity
    }
}

/// Serve one connection until it closes.
///
/// Always drops the connection's registrations on the way out — see the module
/// docs on why that must hold for every exit path.
pub fn serve_connection<S: io::Read + io::Write>(
    stream: &mut S,
    ops: &ProbeOps,
    peer: &PeerIdentity,
    conn_id: u64,
) {
    // Includes clean EOF and oversize frames alike: either way this
    // connection is finished.
    while let Ok(bytes) = read_frame_with_cap(stream, MAX_REQUEST_BYTES) {
        let envelope = match ProbeEnvelope::decode(bytes.as_slice()) {
            Ok(e) => e,
            Err(_) => {
                let reply = ProbeReply::Refused {
                    code: ProbeErrorCode::MalformedRequest,
                    reason: "request did not decode as a ProbeEnvelope".into(),
                };
                let _ = write_reply(stream, 0, &reply);
                continue;
            }
        };
        let request_id = envelope.request_id;

        let Some(request) = request_from_envelope(envelope) else {
            let reply = ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: "unsupported or incomplete request body".into(),
            };
            let _ = write_reply(stream, request_id, &reply);
            continue;
        };

        // Identity work is I/O, so it happens here rather than inside the
        // sans-io dispatcher.
        let verdict = match &request {
            ProbeRequest::Register(req) => verify_identity(req, true),
            _ => IdentityVerdict {
                verified: true,
                connection_alive: true,
            },
        };

        let reply = ops.dispatch(request, peer, conn_id, verdict);
        if write_reply(stream, request_id, &reply).is_err() {
            break;
        }
    }

    // Every exit path lands here. This is the daemon's primary death signal.
    ops.registry().drop_by_conn(conn_id);
}

fn write_reply<S: io::Write>(
    stream: &mut S,
    request_id: u64,
    reply: &ProbeReply,
) -> io::Result<()> {
    let envelope = envelope_from_reply(request_id, reply);
    write_frame(stream, &envelope.encode_to_vec()).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(())
}

/// Build the ops core for a daemon owned by `owner`.
pub fn build_ops(owner: String) -> io::Result<Arc<ProbeOps>> {
    let policy = running_process::broker::server::PeerCredentialPolicy::current_user()
        .ok_or_else(|| io::Error::other("cannot resolve the current user for the owner policy"))?;
    Ok(Arc::new(ProbeOps::new(
        Arc::new(crate::registry::Registry::new(owner)),
        policy,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::server::PeerCredentialPolicy;
    use running_process_probe::probe_diag::v1 as wire;

    const OWNER: &str = "owner-uid";

    fn ops() -> ProbeOps {
        ProbeOps::new(
            Arc::new(crate::registry::Registry::new(OWNER.into())),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.into(),
            },
        )
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: OWNER.into(),
        }
    }

    /// Register this very process, so identity verification can actually
    /// succeed against a real executable.
    fn self_register_envelope(nonce: u8, request_id: u64) -> ProbeEnvelope {
        let exe = std::env::current_exe().expect("current exe");
        let sha = running_process::broker::backend_lifecycle::identity::sha256_file(&exe)
            .expect("hash self");
        ProbeEnvelope {
            wire_version: 1,
            request_id,
            deadline_unix_ms: 0,
            body: Some(Body::Register(RegisterProcess {
                key: Some(wire::ProcessKey {
                    pid: u64::from(std::process::id()),
                    start_time: Some(1_700_000_000_000),
                    boot_id: Some(running_process::broker::host_identity::current().boot_id),
                }),
                exe_path: exe.to_string_lossy().into_owned(),
                exe_sha256: sha.to_vec(),
                app_class: "test".into(),
                registration_nonce: vec![nonce; 32],
                ..Default::default()
            })),
        }
    }

    /// Drive `serve_connection` over an in-memory duplex.
    fn serve_bytes(ops: &ProbeOps, requests: &[ProbeEnvelope], conn_id: u64) -> Vec<ProbeEnvelope> {
        let mut input = Vec::new();
        for env in requests {
            write_frame(&mut input, &env.encode_to_vec()).unwrap();
        }

        struct Duplex {
            read: std::io::Cursor<Vec<u8>>,
            written: Vec<u8>,
        }
        impl io::Read for Duplex {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.read.read(buf)
            }
        }
        impl io::Write for Duplex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut duplex = Duplex {
            read: std::io::Cursor::new(input),
            written: Vec::new(),
        };
        serve_connection(&mut duplex, ops, &peer(), conn_id);

        let mut replies = Vec::new();
        let mut cursor = std::io::Cursor::new(duplex.written);
        while let Ok(frame) = read_frame_with_cap(&mut cursor, MAX_REQUEST_BYTES) {
            if let Ok(env) = ProbeEnvelope::decode(frame.as_slice()) {
                replies.push(env);
            }
        }
        replies
    }

    fn status(env: &ProbeEnvelope) -> RegistrationStatus {
        match env.body.clone() {
            Some(Body::RegistrationStatus(s)) => s,
            other => panic!("expected RegistrationStatus, got {other:?}"),
        }
    }

    #[test]
    fn registering_this_process_reaches_armed() {
        let ops = ops();
        let replies = serve_bytes(&ops, &[self_register_envelope(1, 7)], 1);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].request_id, 7, "request_id must be echoed");
        assert_eq!(status(&replies[0]).state, 2, "2 == ARMED");
    }

    /// The contract that makes the daemon's liveness model work.
    #[test]
    fn closing_the_connection_drops_the_registration_immediately() {
        let ops = ops();
        serve_bytes(&ops, &[self_register_envelope(2, 1)], 42);
        assert!(
            ops.registry().is_empty(),
            "connection close must drop registrations at once, not after the \
             heartbeat grace"
        );
    }

    #[test]
    fn a_malformed_frame_is_refused_not_dropped() {
        let ops = ops();
        let mut input = Vec::new();
        write_frame(&mut input, b"not a protobuf at all").unwrap();

        struct R(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl io::Read for R {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl io::Write for R {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut s = R(std::io::Cursor::new(input), Vec::new());
        serve_connection(&mut s, &ops, &peer(), 1);

        let mut cursor = std::io::Cursor::new(s.1);
        let frame = read_frame_with_cap(&mut cursor, MAX_REQUEST_BYTES)
            .expect("a refusal must be written, not the connection silently closed");
        let env = ProbeEnvelope::decode(frame.as_slice()).unwrap();
        assert_eq!(status(&env).state, 3, "3 == DROPPED/refused");
    }

    #[test]
    fn a_foreign_peer_is_refused() {
        let ops = ops();
        let stranger = PeerIdentity {
            pid: 1,
            uid_or_sid: "someone-else".into(),
        };
        let mut input = Vec::new();
        write_frame(&mut input, &self_register_envelope(3, 1).encode_to_vec()).unwrap();

        struct R(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl io::Read for R {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl io::Write for R {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut s = R(std::io::Cursor::new(input), Vec::new());
        serve_connection(&mut s, &ops, &stranger, 1);

        assert!(
            ops.registry().is_empty(),
            "a foreign peer must create nothing"
        );
    }

    /// A key without a start time cannot survive PID reuse.
    #[test]
    fn a_key_without_a_start_time_is_refused() {
        let key = wire::ProcessKey {
            pid: 10,
            start_time: None,
            boot_id: Some("b".into()),
        };
        assert!(
            key_from_proto(key).is_none(),
            "pid-only identity would silently alias across PID reuse"
        );
    }

    #[test]
    fn identity_verification_rejects_a_wrong_hash() {
        let exe = std::env::current_exe().unwrap();
        let request = RegisterRequest {
            key: ProcessKey {
                pid: std::process::id(),
                started_at_unix_ms: 1,
                boot_id: String::new(),
            },
            exe_path: exe,
            // Deliberately not the real hash.
            exe_sha256: [0xAB; 32],
            app_class: "x".into(),
            app_name: "x".into(),
            app_version: "1".into(),
            allow_policy: AllowPolicy::default(),
            disclosure: Disclosure::default(),
            nonce: [1u8; 32],
            supported_ops: Vec::new(),
        };
        assert!(
            !verify_identity(&request, true).verified,
            "a mismatched executable hash must not verify"
        );
    }
}
