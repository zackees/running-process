//! Windows Actually Portable Executable launch mechanics.
//!
//! An APE image is a native PE executable here, so the kernel never refuses
//! it and none of the recovery runs.

use std::io;
use std::path::Path;

/// An APE image is a native PE executable on Windows.
pub const APE_NEEDS_LOADER: bool = false;

/// No shell is needed to start an APE image on Windows.
pub const APE_SHELL: &str = "";

/// No loader is needed to start an APE image on Windows.
pub const APE_SYSTEM_LOADERS: &[&str] = &[];

/// No loader is needed to start an APE image on Windows.
pub const APE_EMBEDDED_LOADER: bool = false;

/// `CreateProcess` has no shell fallback, and an APE image needs none.
pub const APE_EXECVP_SHELL_FALLBACK: bool = false;

/// Windows runs APE images natively, so no spawn error is a refused one.
pub fn is_exec_format_error(_error: &io::Error) -> bool {
    false
}

/// Windows decides executability by extension, not by a permission bit.
pub fn mark_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Never reached: no Windows spawn error is retried as an APE image.
pub fn route_through_execvp(_command: &mut std::process::Command) {}

/// Never reached: no Windows spawn error is retried as an APE image.
#[cfg(feature = "async-process")]
pub fn route_tokio_through_execvp(_command: &mut tokio::process::Command) {}
