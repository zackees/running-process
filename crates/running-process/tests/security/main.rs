//! Security regression tests for currently exposed v1 broker validation.
//!
//! These tests intentionally cover only Phase 0/1 surfaces. Broker bind,
//! peer-credential, and handle-release checks are documented as ignored
//! placeholders until those runtime surfaces exist.

#![cfg(feature = "client")]

mod deferred_runtime_surfaces;
mod pipe_name_validation;
mod service_name_validation;
mod wanted_version_shell_injection;
mod wanted_version_traversal;
