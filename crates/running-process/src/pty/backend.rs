//! PTY backend abstraction (#150).
//!
//! `native_pty_process.rs` was riddled with `#[cfg(windows)]` /
//! `#[cfg(unix)]` branches around the underlying portable-pty calls.
//! After the #150 rewrite we have two distinct backends:
//!
//! * Windows — `conpty_passthrough` (raw ConPTY via windows-sys with
//!   `PSEUDOCONSOLE_PASSTHROUGH_MODE` enabled)
//! * Unix — portable-pty's POSIX backend (unchanged)
//!
//! The `Backend` type alias resolves to one or the other per-target,
//! and `native_pty_process.rs` makes a single `Backend::openpty(...)`
//! call instead of branching.
//!
//! Wave 1 stub. Trait definition + concrete `Backend` alias land in
//! W4; W5 swaps `native_pty_process.rs` over.
