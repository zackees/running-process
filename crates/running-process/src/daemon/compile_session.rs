//! Daemon-side compile-session handler (soldr#2365).
//!
//! The implementation moved to [`crate::broker::session_takeover`] (on the
//! `client-async` feature) so async consumer daemons that enable
//! `running-process/client-async` but not `daemon` — notably soldr-daemon — can
//! serve SESSION without pulling the full running-process daemon runtime. This
//! module re-exports it unchanged so daemon-side call sites and tests keep their
//! `daemon::compile_session::*` paths.

pub use crate::broker::session_takeover::{
    run_compile_session, serve_session, session_framed, session_takeover_from_buffered,
    SessionFrameCodec,
};

#[cfg(test)]
mod tests;
