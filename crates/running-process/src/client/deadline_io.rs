//! Deadline-bounded connect + frame-length-cap helpers for the sync IPC
//! clients (issue #590, cluster B).
//!
//! `interprocess` local sockets expose no portable connect or read
//! timeout, and none of the sync clients under this module capped the
//! 4-byte length prefix before allocating. That left two ways for a
//! bound-but-unaccepting, crashed, or hostile peer to wedge or exhaust the
//! Python-facing client:
//!
//! - **Connect wedge:** `Stream::connect` is a blocking syscall; a socket
//!   that exists but whose server never completes the accept parks the
//!   caller indefinitely. [`connect_with_timeout`] runs the blocking
//!   connect on a helper thread and bounds it (mirrors the proven
//!   `broker::backend_lifecycle::probe` idiom).
//! - **Huge-alloc / unbounded read:** a bogus length prefix (e.g.
//!   `0xFFFF_FFFF`) drove a multi-GiB `vec![0u8; len]` followed by a read
//!   for bytes that never arrive. [`check_frame_len`] rejects any length
//!   over [`MAX_FRAME_BYTES`] before allocating, matching what the broker
//!   framing layer already enforces.

use crate::broker::protocol::framing::MAX_FRAME_BYTES;
use crate::client::paths;
use interprocess::local_socket::Stream;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Default connect timeout for the sync IPC clients. Generous enough not
/// to trip on a briefly-busy daemon, but bounded so a permanently
/// non-accepting socket can't wedge the caller. Override with
/// `RUNNING_PROCESS_CLIENT_CONNECT_TIMEOUT_MS` (milliseconds).
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT_ENV: &str = "RUNNING_PROCESS_CLIENT_CONNECT_TIMEOUT_MS";

fn connect_timeout() -> Duration {
    std::env::var(CONNECT_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

/// Connect to `socket_path` on a helper thread, bounded by
/// [`connect_timeout`]. On timeout the helper thread retains ownership of
/// (and eventually drops) the abandoned connection attempt — the same
/// leak-on-timeout pattern the broker probe path uses — so a wedged
/// listener never wedges the caller.
pub(crate) fn connect_with_timeout(socket_path: &str) -> io::Result<Stream> {
    let path = socket_path.to_string();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("rp-client-connect".to_string())
        .spawn(move || {
            use interprocess::local_socket::traits::Stream as _;
            let result = match paths::make_socket_name(&path) {
                Ok(name) => Stream::connect(name),
                Err(err) => Err(err),
            };
            // Receiver gone means the caller timed out; drop the stream here.
            let _ = tx.send(result);
        })?;

    match rx.recv_timeout(connect_timeout()) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "daemon socket connect timed out after {:?}: the socket \
                 exists but the server never accepted the connection \
                 (path {socket_path})",
                connect_timeout()
            ),
        )),
    }
}

/// Reject a frame length larger than [`MAX_FRAME_BYTES`] before it is used
/// to allocate a receive buffer, so a corrupt/hostile/desynced peer cannot
/// trigger a multi-GiB allocation followed by an unbounded read for bytes
/// that never arrive.
pub(crate) fn check_frame_len(len: usize) -> io::Result<()> {
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_frame_len_accepts_within_cap() {
        assert!(check_frame_len(0).is_ok());
        assert!(check_frame_len(MAX_FRAME_BYTES).is_ok());
    }

    #[test]
    fn check_frame_len_rejects_over_cap() {
        let err = check_frame_len(MAX_FRAME_BYTES + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn check_frame_len_rejects_bogus_u32_max() {
        // A desynced peer sending 0xFFFF_FFFF must be rejected before the
        // ~4 GiB allocation, not after.
        assert!(check_frame_len(u32::MAX as usize).is_err());
    }

    #[test]
    fn connect_with_timeout_errors_on_missing_socket() {
        // A path that does not exist should fail promptly (connection
        // refused / not found), never hang.
        let bogus = if cfg!(windows) {
            r"\\.\pipe\running-process-nonexistent-test-endpoint"
        } else {
            "/tmp/running-process-nonexistent-test-endpoint.sock"
        };
        assert!(connect_with_timeout(bogus).is_err());
    }
}
