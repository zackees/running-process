//! Integration tests for the v1 broker proto schemas (#228 Phase 0).
//!
//! All gated behind `feature = "client"` because the broker module
//! itself is. With `--no-default-features` this crate compiles to an
//! empty integration test binary.

#![cfg(feature = "client")]

mod admin;
mod backend_handle_boot_id;
mod backend_handle_common;
mod backend_handle_dead;
mod backend_handle_probe;
mod backend_handle_recycled;
mod backend_registry;
mod client;
mod connection;
mod framing;
mod hello_handler;
mod instance;
mod lifecycle_event_size;
mod manifest_atomic;
mod manifest_boot_id;
mod manifest_corruption;
mod manifest_roundtrip;
mod metrics_names_frozen;
mod names;
mod perf_guard;
mod proto_field_numbers;
mod proto_roundtrip;
mod serve;
mod service_def_loader;
mod trace_propagation;
mod verify_pid;
