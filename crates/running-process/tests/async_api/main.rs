//! Async process-API integration tests (#1158).
//!
//! The tokio-facing `async-process` surface: owner-death propagation,
//! sync/async parity, the async process API itself, and semantic capture.
//! Each former top-level `tests/*.rs` file is a module here, so the
//! category links one test executable instead of four. Test IDs are
//! `async_api::<module>::<test_name>`.
//!
//! Every member is gated on `feature = "async-process"` (and
//! `async_parity_test` additionally on `feature = "pty"`), kept as the
//! inner `#![cfg(...)]` attribute at the top of each file.

mod async_owner_death_test;
mod async_parity_test;
mod async_process_test;
mod async_semantic_capture;
