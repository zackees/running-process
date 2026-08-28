//! PTY substrate integration tests (#1158).
//!
//! The PTY master/slave surface, the MITM stdin/paste substrate, terminal
//! capability probing, and the daemon-side session attach paths (which are
//! PTY-substrate tests despite their `daemon_` prefix). Each former
//! top-level `tests/*.rs` file is a module here, so the whole category
//! links one test executable instead of ten. Test IDs are
//! `pty::<module>::<test_name>`.
//!
//! Per-module feature gates stay as the inner `#![cfg(...)]` attribute at
//! the top of each file, the same convention `tests/broker/` uses.

#[path = "../common/mod.rs"]
mod common;

mod daemon_cross_process_pty_attach_test;
mod daemon_non_tty_attach_test;
mod daemon_pipe_session_attach_test;
mod daemon_pty_session_attach_test;
mod interactive_pty_session_test;
mod pty_conhost_job_test;
mod pty_master_public_api_test;
mod pty_mitm_paste_test;
mod pty_mitm_stdin_test;
mod pty_unix_teardown_test;
mod stdio_fidelity_oracle_test;
mod terminal_graphics_capabilities_test;
mod window_icon_x11_test;
