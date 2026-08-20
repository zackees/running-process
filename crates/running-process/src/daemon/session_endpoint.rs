//! Phase 3 daemon SESSION backend endpoint (soldr#2365).
//!
//! The daemon's **real serve surface** for compile sessions, on a **separate,
//! broker-facing backend endpoint** (per the #2386 ruling — the legacy
//! `DaemonRequest` `handle_connection` path is untouched; the slice-6 cutover is
//! deleting one listener, not un-weaving a shared loop). The broker binds this
//! endpoint and hands the daemon the listener (`broker_owned_bind`); this loop
//! accepts each SESSION connection and serves it via
//! [`serve_session`], which reads
//! the connection's opening `SessionStart` frame, builds the contained compiler
//! child, and proxies its stdio on the SESSION wire.
//!
//! Sessions run **concurrently**, each in its own contained process group (its
//! own Job Object on Windows / process group on Unix), so one session's child
//! never shares a kill domain with another's. Unix-first; the Windows
//! named-pipe path uses the same `interprocess` abstraction.

use std::sync::Arc;

use crate::containment::ContainedProcessGroup;
use crate::daemon::compile_session::{serve_session, session_framed};
use crate::daemon::IntoDaemonAsyncListener;

/// Accept SESSION connections on `listener` and serve each as a compile session
/// until the listener errors.
///
/// Each accepted connection is spawned as an independent task with its own
/// [`ContainedProcessGroup`]; a session that fails to spawn its group or hits a
/// transport/protocol error is dropped without taking down the accept loop.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable); per-
/// connection errors never propagate here.
pub async fn serve_session_endpoint<L>(listener: L) -> std::io::Result<()>
where
    L: IntoDaemonAsyncListener,
{
    let listener = listener.into_async_listener();
    loop {
        let stream = listener.accept().await?;
        tokio::spawn(async move {
            let Ok(group) = ContainedProcessGroup::new() else {
                return;
            };
            let _ = serve_session(session_framed(stream), Arc::new(group)).await;
        });
    }
}

#[cfg(test)]
mod tests;
