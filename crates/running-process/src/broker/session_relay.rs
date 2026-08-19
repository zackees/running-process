//! Phase 3 broker → daemon SESSION relay (soldr#2365).
//!
//! The broker's **full-proxy** of a compile session — it stays in the middle and
//! relays every byte between the client and the daemon's SESSION backend
//! endpoint ([`crate::daemon::session_endpoint`]), rather than handing the
//! connection off and stepping out. The client speaks the SESSION wire
//! end-to-end; the broker is transparent to it. Hello / routing / token
//! negotiation runs *before* this and selects which daemon endpoint to dial.
//!
//! [`relay_local_socket_session`] is the production transport. Linux moves each
//! direction through one bounded nonblocking kernel pipe with `splice(2)`;
//! Windows and macOS retain Tokio's buffered relay. [`relay_session`] remains
//! the generic buffered reference and pre-transfer fallback.

use interprocess::local_socket::tokio::prelude::*;

/// Resolve a daemon SESSION endpoint path into an `interprocess` name
/// (filesystem path on Unix, pipe namespace on Windows), matching how the daemon
/// binds it.
fn daemon_endpoint_name(path: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    crate::broker::server::singleton_bind::wrap_socket_name(path).map_err(std::io::Error::other)
}

/// Relay an arbitrary async client's SESSION connection with bounded userspace
/// buffers.
///
/// This is the portable reference implementation and the Linux fallback when
/// raw-descriptor preparation fails before any payload byte moves. Production
/// local-socket accept loops should call [`relay_local_socket_session`] so Linux
/// can use its measured splice path.
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

/// Relay a production `interprocess` SESSION connection to its selected daemon.
///
/// The broker does not parse the SESSION wire: `SessionStart`, stdio, and
/// `SessionExit` remain byte-transparent. Linux uses `splice(2)` after both
/// directions have acquired every descriptor and pipe they need. If that
/// preparation fails, the untouched streams fall back to buffered I/O. A fatal
/// error after the first splice is returned rather than replaying bytes.
/// Windows and macOS always use the buffered implementation.
///
/// # Errors
///
/// Fails if the daemon endpoint cannot be dialed, or on a fatal transport error
/// during the relay.
pub async fn relay_local_socket_session(
    client: interprocess::local_socket::tokio::Stream,
    daemon_socket_path: &str,
) -> std::io::Result<()> {
    let name = daemon_endpoint_name(daemon_socket_path)?;
    let daemon = interprocess::local_socket::tokio::Stream::connect(name).await?;
    running_process_platform_internal::relay_local_socket_session(client, daemon).await
}

// The relay's e2e test dials a real daemon SESSION endpoint
// (`serve_session_endpoint`), which only exists under `daemon`; the relay
// module itself needs only `client-async`.
#[cfg(all(test, feature = "daemon"))]
mod tests;
