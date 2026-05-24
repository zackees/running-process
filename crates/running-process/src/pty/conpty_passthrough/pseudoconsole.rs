//! `HPCON` (pseudo-console handle) wrapper (#150 W2).
//!
//! Ports `portable-pty-0.9.0/src/win/psuedocon.rs` to use windows-sys
//! directly. Adds `PSEUDOCONSOLE_PASSTHROUGH_MODE` to the
//! `CreatePseudoConsole` flags so child bytes flow through unmodified
//! (vs portable-pty's default virtual-screen synthesis).
//!
//! Wave 1 stub. Real implementation in W2.

#![cfg(windows)]
