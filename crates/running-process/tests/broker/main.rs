//! Integration tests for the v1 broker proto schemas (#228 Phase 0).
//!
//! All gated behind `feature = "client"` because the broker module
//! itself is. With `--no-default-features` this crate compiles to an
//! empty integration test binary.

#![cfg(feature = "client")]

mod admin;
mod backend_endpoint_allocator;
mod backend_handle_boot_id;
mod backend_handle_common;
mod backend_handle_dead;
mod backend_handle_probe;
mod backend_handle_recycled;
mod backend_registry;
mod broadcast_release_handles;
mod client;
mod connection;
mod contrib_templates;
mod docs_escape_hatch;
mod docs_index;
mod framing;
mod handoff_backend_lib;
mod handoff_fallback_perm_denied;
mod handoff_token_mismatch;
mod handoff_transport;
mod handoff_under_load;
mod hello_concurrent;
mod hello_handler;
mod hello_rate_limit;
mod hello_router;
mod hello_service_unknown;
mod hello_skip;
mod hello_version_blocked;
mod instance;
mod instance_isolation;
mod lifecycle_event_size;
mod manifest_atomic;
mod manifest_boot_id;
mod manifest_corruption;
mod manifest_roundtrip;
mod metrics_names_frozen;
mod names;
mod peer_creds_drop;
mod perf_guard;
mod proto_field_numbers;
mod proto_roundtrip;
mod recovery_one_retry;
mod serve;
mod service_def_loader;
mod spawn_coordinator;
mod spawn_wait;
mod trace_propagation;
mod verify_pid;
