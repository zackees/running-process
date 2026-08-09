//! Phase 3 broker → daemon SESSION relay (soldr#2365).
//!
//! The broker's **full-proxy** of a compile session — it stays in the middle and
//! relays every byte between the client and the daemon's SESSION backend
//! endpoint ([`crate::daemon::session_endpoint`]), rather than handing the
//! connection off and stepping out (the legacy `handoff_serve` model the #2360
//! design replaces). The client speaks the SESSION wire end-to-end; the broker
//! is transparent to it. Hello / routing / token negotiation runs *before* this
//! and selects which daemon endpoint to dial.
//!
//! [`relay_session`] dials the daemon's SESSION endpoint and pumps bytes both
//! ways with [`tokio::io::copy_bidirectional`], whose fixed per-direction buffers
//! bound per-lane memory on both relay legs. Unix-first; the Windows named-pipe
//! path uses the same `interprocess` abstraction.

use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;

/// Resolve a daemon SESSION endpoint path into an `interprocess` name
/// (filesystem path on Unix, pipe namespace on Windows), matching how the daemon
/// binds it.
fn daemon_endpoint_name(path: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::ToFsName;
        path.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::ToNsName;
        path.to_ns_name::<GenericNamespaced>()
    }
}

/// Relay a client's SESSION connection to the daemon endpoint at
/// `daemon_socket_path`, proxying bytes both ways until either side closes.
///
/// The broker does not parse the SESSION wire — the client's opening
/// `SessionStart` and its stdio flow through to the daemon verbatim, and the
/// daemon's stdout/stderr/exit flow back. Returns once either leg reaches EOF.
///
/// # Errors
///
/// Fails if the daemon endpoint cannot be dialed, or on a fatal transport error
/// during the relay.
pub async fn relay_session<C>(mut client: C, daemon_socket_path: &str) -> std::io::Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let name = daemon_endpoint_name(daemon_socket_path)?;
    let mut daemon = interprocess::local_socket::tokio::Stream::connect(name).await?;
    tokio::io::copy_bidirectional(&mut client, &mut daemon).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
