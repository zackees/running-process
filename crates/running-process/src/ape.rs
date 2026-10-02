//! Actually Portable Executable (APE / cosmocc) support.
//!
//! Cosmopolitan APE binaries start with a shell prologue rather than a native
//! image header. A host without a `binfmt_misc` registration for them -- stock
//! NixOS among them -- refuses them with `ENOEXEC` ("Exec format error"), and
//! only a shell knows to retry.
//!
//! This crate launches them through a loader: an explicit one
//! ([`LOADER_ENV`]), else the loader embedded in the image (the default
//! `ape-loader` feature; installed into a private, exec-capable cache, or a
//! sealed memfd), else an installed `ape`, else the host shell.
//!
//! - Spawns whose program and environment are known plan the loader before
//!   the spawn: `SpawnSpec`/`AsyncProcess`, argv `NativeProcess`es, and
//!   [`command`] / `tokio_command` for callers building their own command.
//! - A caller-built command keeps every caller setting and is retried once
//!   after a refusal, through `execvp`'s POSIX `ENOEXEC` shell rule
//!   ([`spawn_std`]).
//! - [`fork_guard`] / [`exclusive_fork_guard`] are the process-wide fork lock
//!   that keeps a freshly written loader out of concurrently forked children
//!   (`ETXTBSY`); a spawner outside this crate should hold [`fork_guard`]
//!   across its spawn.

pub use running_process_platform_internal::platform::ape::{
    command, embedded_loader, exclusive_fork_guard, fork_guard, is_ape_file, is_ape_header,
    is_exec_format_error, loader_blob_range, plan_launch, prepare_std_retry, resolve_program,
    retry_while_busy, spawn_std, ApeLaunch, ApeOptions, LoaderKind, CACHE_DIR_ENV, LOADER_ENV,
    MAGICS, NEEDS_LOADER,
};
#[cfg(feature = "ape-loader")]
pub use running_process_platform_internal::platform::ape::{extract_loader, gunzip};
#[cfg(feature = "async-process")]
pub use running_process_platform_internal::platform::ape::{prepare_tokio_retry, tokio_command};
