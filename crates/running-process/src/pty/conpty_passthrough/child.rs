//! Spawned-process handle wrapper for ConPTY children (#150 W2).
//!
//! Owns the Windows process HANDLE returned by `CreateProcessW` and
//! exposes the narrow API `native_pty_process.rs` needs:
//! `pid()`, `try_wait()`, `wait()`, `kill()`, `as_raw_handle()`.
//!
//! Wave 1 stub. Real implementation in W2.

#![cfg(windows)]
