//! Core process-substrate integration tests (#1158).
//!
//! Spawning, containment, filesystem adversarial cases, and originator
//! tagging. Each former top-level `tests/*.rs` file is a module here, so
//! the whole category links one test executable instead of five. Test IDs
//! are `core::<module>::<test_name>`.
//!
//! Per-module feature gates stay as the inner `#![cfg(...)]` attribute at
//! the top of each file, the same convention `tests/broker/` uses.

mod containment_test;
mod fs_adversarial_test;
mod observer_launched_tree_test;
mod originator_test;
mod probe_facade_surface;
mod process_core_test;
mod process_watch_exact;
mod spawn_test;
