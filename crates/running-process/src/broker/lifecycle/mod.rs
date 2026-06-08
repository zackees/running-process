//! Per-user, per-machine derivations for broker pipe naming and SID
//! hashing.
//!
//! Phase 1 of #228 (issue #230). Two submodules:
//!
//! - [`sid`] — derives a 16-character hex blake3 hash of the caller's
//!   user identity. On Windows that is the user's SID (from
//!   `OpenProcessToken` → `GetTokenInformation(TokenUser)`). On Linux
//!   it is `format!("{uid}:{machine_id}")`. On macOS it is
//!   `format!("{uid}:{IOPlatformUUID}")`.
//! - [`names`] — uses the SID hash to build the four canonical broker
//!   pipe names defined in #228.

pub mod crash_dump;
pub mod names;
pub mod privilege;
pub mod sid;

pub use names::{
    backend_pipe, explicit_instance_pipe, private_broker_pipe, shared_broker_pipe, PipePath,
    PipePathError,
};
pub use crash_dump::{CrashDumpError, CRASH_DUMP_DIR_ENV};
pub use privilege::{
    refuse_privileged_run, PrivilegeError, PrivilegedIdentity, ALLOW_PRIVILEGED_ENV,
};
pub use sid::{user_sid_hash, SidError};
