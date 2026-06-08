//! Integration tests for the v1 broker proto schemas (#228 Phase 0).
//!
//! All gated behind `feature = "client"` because the broker module
//! itself is. With `--no-default-features` this crate compiles to an
//! empty integration test binary.

#![cfg(feature = "client")]

mod framing;
mod lifecycle_event_size;
mod names;
mod proto_field_numbers;
mod proto_roundtrip;
