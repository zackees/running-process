//! Security regression tests for currently exposed v1 broker validation.
//!
//! These tests intentionally cover the broker security surfaces that are
//! available without external reviewer or cross-user setup. Deferred runtime
//! surfaces stay documented separately until their broker runtime exists.

#![cfg(feature = "client")]

mod cross_user_release_handles;
mod deferred_runtime_surfaces;
mod manifest_tamper_detection;
mod pipe_name_validation;
mod pipe_squatting;
mod service_name_validation;
mod wanted_version_shell_injection;
mod wanted_version_traversal;
