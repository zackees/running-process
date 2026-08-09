//! Phase 3 daemon backend endpoint (soldr#2365): the mux dispatch that finally
//! wires the SESSION handler onto a real [`BackendEndpointMux`] backend.
//!
//! This is the slice [`crate::daemon::compile_session`] names in its docs ("the
//! next slice registers this lane on a real `BackendEndpointMux` backend in the
//! daemon's accept loop"). A daemon's single backend endpoint serves two kinds
//! of traffic here:
//!
//! 1. **`BackendHandle` identity probes** — the nonce challenge the broker's
//!    [`crate::broker::backend_handle::BackendHandle::probe_with_service`] sends
//!    to verify a launched/registered daemon. The mux answers them from the
//!    daemon's own [`DaemonProcess`] identity, so backend registration works
//!    against this endpoint.
//! 2. **SESSION (`0x5350`) frames** — a compile session's stdio stream. The
//!    first `0x5350` frame marks the connection as a session for its lifetime
//!    (**streaming takeover**, matching the daemon's other takeover handlers):
//!    the connection is handed to [`serve_session`], which reads the opening
//!    `SessionStart` and proxies the contained child's stdio byte-for-byte.
//!
//! The SESSION wire is Model B — each `SessionFrame` wrapped in a `Frame` on the
//! `0x5350` lane ([`SessionFrameCodec`]) — exactly what the mux classifies and
//! what the broker's transparent relay carries end-to-end. There is no legacy
//! wire on this endpoint, so the mux's legacy detector always answers
//! `NotLegacy`.

use std::sync::Arc;

use bytes::{Buf, BytesMut};
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::broker::backend_handle::DaemonProcess;
use crate::broker::backend_sdk::{BackendEndpointMux, LegacyClassification, MuxPoll};
use crate::broker::protocol::registry::SESSION_PAYLOAD_PROTOCOL;
use crate::containment::ContainedProcessGroup;
use crate::daemon::compile_session::session_takeover_from_buffered;

/// Accept connections on `listener` and serve each backend connection with the
/// daemon `identity` (used to answer identity probes).
///
/// Each connection runs independently in its own task with its own contained
/// process group, so one session never shares a kill domain with another.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable). Per-
/// connection errors are logged and never propagate.
pub async fn serve_backend_endpoint(
    listener: interprocess::local_socket::tokio::Listener,
    identity: DaemonProcess,
) -> std::io::Result<()> {
    loop {
        let stream = listener.accept().await?;
        let identity = identity.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_backend_connection(stream, &identity).await {
                eprintln!("running-process-daemon: backend connection ended: {err}");
            }
        });
    }
}

/// Serve one backend connection: classify each frame with a
/// [`BackendEndpointMux`], answer identity probes inline, and hand the
/// connection to [`serve_session`] on the first SESSION (`0x5350`) frame.
///
/// # Errors
///
/// Fails on a transport error, an unexpected non-SESSION/non-probe frame, or a
/// failure propagated by the compile-session handler.
pub async fn serve_backend_connection<T>(mut io: T, identity: &DaemonProcess) -> std::io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // No legacy wire on the SESSION backend endpoint: every frame is either a
    // `BackendHandle` probe or a `0x5350` SESSION frame.
    let mux = BackendEndpointMux::new(identity.clone(), &[SESSION_PAYLOAD_PROTOCOL], |_buf| {
        LegacyClassification::NotLegacy
    });

    let mut buf = BytesMut::new();
    loop {
        let verdict = mux
            .poll(&buf)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
        match verdict {
            MuxPoll::NeedMoreBytes => {
                if io.read_buf(&mut buf).await? == 0 {
                    // Peer hung up before completing a frame — a bare probe
                    // that closed, or an idle connection. Not an error.
                    return Ok(());
                }
            }
            MuxPoll::Legacy => {
                return Err(std::io::Error::other(
                    "unexpected legacy-wire bytes on the SESSION backend endpoint",
                ));
            }
            MuxPoll::ProbeAnswered { reply, consumed } => {
                io.write_all(&reply).await?;
                io.flush().await?;
                buf.advance(consumed);
                // A probe is a one-shot identity check; the peer may probe
                // again or send a SESSION frame next, so keep classifying.
            }
            MuxPoll::Payload { .. } => {
                // A `0x5350` frame → this connection is a compile session for
                // its lifetime. Hand it off with the buffered bytes intact (do
                // NOT consume them) so the takeover handler reads the opening
                // `SessionStart` itself.
                let group = Arc::new(ContainedProcessGroup::new()?);
                session_takeover_from_buffered(io, buf, group).await?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
