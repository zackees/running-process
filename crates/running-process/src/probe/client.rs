//! Transport for the probe client.
//!
//! One request/reply round trip per call over the daemon's framed control
//! socket. Every operation is deadline-bounded: a daemon that has bound its
//! socket but stopped accepting must not be able to wedge the calling
//! application, which is the failure mode an unbounded blocking read invites.

use std::io;
use std::time::Duration;

use prost::Message as _;
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, Heartbeat, ProbeEnvelope, ProcessKey, RegisterProcess,
    RegistrationStatus, UnregisterProcess,
};

use crate::broker::protocol::framing::{read_frame_with_cap, write_frame, MAX_FRAME_BYTES};

/// Cap on a single reply. Registration replies are small; anything larger is a
/// malformed or hostile peer, and the cap bounds the allocation before it
/// happens.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Why a client operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon could not be reached.
    #[error("probe daemon unreachable: {0}")]
    Unreachable(#[source] io::Error),
    /// Wire-level failure (framing, encode, decode).
    #[error("probe wire error: {0}")]
    Wire(String),
    /// The daemon refused the request.
    #[error("probe daemon refused the request: {reason}")]
    Refused {
        /// Human-readable reason from the daemon.
        reason: String,
    },
    /// The daemon replied with something other than the expected message.
    #[error("unexpected reply from probe daemon")]
    UnexpectedReply,
}

/// The operations a probe client performs.
///
/// A trait so tests can drive the worker with an in-memory fake, and so a
/// later slice can supply a different transport without touching the worker's
/// reconnect and heartbeat logic.
pub trait ProbeClient: Send {
    /// Enroll this process; returns the identity the daemon armed.
    fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError>;
    /// Refresh liveness.
    fn heartbeat(&mut self, key: &ProcessKey) -> Result<(), ClientError>;
    /// Best-effort deregistration.
    fn unregister(&mut self, key: &ProcessKey) -> Result<(), ClientError>;
}

/// A [`ProbeClient`] over the daemon's local control socket.
#[derive(Debug)]
pub struct SocketProbeClient {
    stream: interprocess::local_socket::Stream,
    request_id: u64,
}

impl SocketProbeClient {
    /// Connect to the daemon at `socket_path`, bounding the attempt by
    /// `deadline`.
    pub fn connect(socket_path: &str, deadline: Duration) -> Result<Self, ClientError> {
        use interprocess::local_socket::traits::Stream as _;

        let name = crate::broker::server::local_socket_name(socket_path)
            .map_err(|e| ClientError::Wire(format!("socket name: {e}")))?;
        let stream =
            interprocess::local_socket::Stream::connect(name).map_err(ClientError::Unreachable)?;

        // Bound receives. Without this a daemon that accepts and then stalls
        // would hold the worker thread forever. interprocess exposes only a
        // recv timeout; the send side is bounded in practice because requests
        // are small and the daemon reads promptly.
        stream
            .set_recv_timeout(Some(deadline))
            .map_err(ClientError::Unreachable)?;

        Ok(Self {
            stream,
            request_id: 0,
        })
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    fn round_trip(&mut self, body: Body) -> Result<ProbeEnvelope, ClientError> {
        let envelope = ProbeEnvelope {
            wire_version: 1,
            request_id: self.next_request_id(),
            deadline_unix_ms: 0,
            body: Some(body),
        };

        write_frame(&mut self.stream, &envelope.encode_to_vec())
            .map_err(|e| ClientError::Wire(e.to_string()))?;

        let bytes = read_frame_with_cap(&mut self.stream, MAX_REPLY_BYTES.min(MAX_FRAME_BYTES))
            .map_err(|e| ClientError::Wire(e.to_string()))?;

        ProbeEnvelope::decode(bytes.as_slice())
            .map_err(|e| ClientError::Wire(format!("decode reply: {e}")))
    }
}

impl ProbeClient for SocketProbeClient {
    fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError> {
        let reply = self.round_trip(Body::Register(req.clone()))?;
        match reply.body {
            Some(Body::RegistrationStatus(RegistrationStatus { state, detail, .. })) => {
                // 2 == ARMED in the probe_diag.v1 RegistrationStatus.State enum.
                if state == 2 {
                    req.key.clone().ok_or(ClientError::UnexpectedReply)
                } else {
                    Err(ClientError::Refused { reason: detail })
                }
            }
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    fn heartbeat(&mut self, key: &ProcessKey) -> Result<(), ClientError> {
        self.round_trip(Body::Heartbeat(Heartbeat {
            key: Some(key.clone()),
        }))?;
        Ok(())
    }

    fn unregister(&mut self, key: &ProcessKey) -> Result<(), ClientError> {
        self.round_trip(Body::Unregister(UnregisterProcess {
            key: Some(key.clone()),
        }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// In-memory client for driving the worker without a daemon.
    #[derive(Default)]
    pub(crate) struct FakeClient {
        pub registered: Arc<Mutex<u32>>,
        pub heartbeats: Arc<Mutex<u32>>,
        pub unregistered: Arc<Mutex<u32>>,
        pub fail_register: bool,
    }

    impl ProbeClient for FakeClient {
        fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError> {
            if self.fail_register {
                return Err(ClientError::Refused {
                    reason: "test".into(),
                });
            }
            *self.registered.lock().unwrap() += 1;
            req.key.clone().ok_or(ClientError::UnexpectedReply)
        }
        fn heartbeat(&mut self, _key: &ProcessKey) -> Result<(), ClientError> {
            *self.heartbeats.lock().unwrap() += 1;
            Ok(())
        }
        fn unregister(&mut self, _key: &ProcessKey) -> Result<(), ClientError> {
            *self.unregistered.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn connect_to_a_nonexistent_socket_is_unreachable_not_a_hang() {
        let err = SocketProbeClient::connect(
            if cfg!(windows) {
                r"\\.\pipe\rp-probe-definitely-not-bound-633"
            } else {
                "/tmp/rp-probe-definitely-not-bound-633.sock"
            },
            Duration::from_millis(100),
        )
        .expect_err("must not connect");
        assert!(matches!(err, ClientError::Unreachable(_)), "{err:?}");
    }

    #[test]
    fn fake_client_round_trips_for_worker_tests() {
        let mut c = FakeClient::default();
        let key = ProcessKey {
            pid: 1,
            start_time: Some(2),
            boot_id: Some("b".into()),
        };
        let req = RegisterProcess {
            key: Some(key.clone()),
            ..Default::default()
        };
        assert_eq!(c.register(&req).unwrap(), key);
        c.heartbeat(&key).unwrap();
        c.unregister(&key).unwrap();
        assert_eq!(*c.registered.lock().unwrap(), 1);
        assert_eq!(*c.heartbeats.lock().unwrap(), 1);
        assert_eq!(*c.unregistered.lock().unwrap(), 1);
    }
}
