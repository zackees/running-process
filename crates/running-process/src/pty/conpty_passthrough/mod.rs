//! Direct ConPTY backend with `PSEUDOCONSOLE_PASSTHROUGH_MODE`.
//!
//! See #150. `portable_pty 0.9.0` hardcodes ConPTY flags to
//! `PSUEDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK |
//! PSEUDOCONSOLE_WIN32_INPUT_MODE` with no API to add
//! `PSEUDOCONSOLE_PASSTHROUGH_MODE = 0x8`. Without passthrough,
//! ConPTY runs a virtual screen: child writes are rendered into a
//! virtual buffer and only ConPTY's synthesized re-emission reaches
//! the master, breaking byte-exact ANSI propagation to the daemon
//! ring buffer (#150 M5 follow-up).
//!
//! This module ports portable-pty's three Windows source files to
//! windows-sys directly and adds the passthrough flag. Public surface
//! mirrors what `native_pty_process.rs` needs: `openpty(size)` returning
//! a `ConPtyPair { master, slave }`, with master/slave types exposing
//! `try_clone_reader`, `take_writer`, `resize`, and `spawn_command`.
//!
//! Wave 1 of #150 (scaffolding only): module exists but is otherwise
//! empty. Implementation lands in W2/W3.

#![cfg(windows)]

// PSEUDOCONSOLE_PASSTHROUGH_MODE is the whole point of this rewrite.
// windows-sys 0.59 does not expose this constant; declared locally
// from the Microsoft consoleapi.h header value.
pub(super) const PSEUDOCONSOLE_PASSTHROUGH_MODE: u32 = 0x8;

pub(super) mod child;
pub(super) mod pipes;
pub(super) mod proc_thread_attr;
pub(super) mod pseudoconsole;
