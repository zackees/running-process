//! `STARTUPINFOEXW`-friendly proc-thread attribute list wrapper (#150 W2).
//!
//! Ports `portable-pty-0.9.0/src/win/procthreadattr.rs` to use
//! windows-sys directly. The attribute we care about is
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, which routes the spawned
//! child's stdio through our `HPCON`.
//!
//! Wave 1 stub. Real implementation in W2.

#![cfg(windows)]
