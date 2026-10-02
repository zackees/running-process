//! Actually Portable Executable (APE) support.
//!
//! Cosmopolitan APE binaries start with a shell prologue rather than a native
//! image header. A host without a `binfmt_misc` registration for them -- stock
//! NixOS among them -- refuses them with `ENOEXEC` ("Exec format error"), and
//! only a shell knows to retry. Every spawn path in this crate retries the way
//! a shell would, after the kernel has refused the image, so a native
//! executable pays nothing:
//!
//! - an argv spawn whose environment is known runs the image through the
//!   loader from [`plan_launch`]: an installed `ape`, else the loader embedded
//!   in the image (the default `ape-loader` feature), else the host shell;
//! - a caller-built command keeps every caller setting and is re-spawned
//!   through `execvp`, whose POSIX `ENOEXEC` rule runs the image's own
//!   prologue, after the embedded loader has been placed where that prologue
//!   looks for it.
//!
//! These functions expose the same decisions for a caller that launches
//! processes some other way; [`spawn_std`] applies the caller-built retry to
//! any `std::process::Command`.

pub use running_process_platform_internal::platform::ape::{
    is_ape_file, is_ape_header, is_exec_format_error, materialize_embedded_loader, plan_launch,
    prepare_std_retry, resolve_program, retry_while_busy, spawn_std, ApeLaunch, ChildEnvironment,
    LoaderKind, MAGICS, NEEDS_LOADER,
};
