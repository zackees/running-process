//! linux executable naming and image-relative discovery.

use std::path::PathBuf;

/// File-name extension the host requires on a runnable image, if any.
pub const EXECUTABLE_EXTENSION: Option<&str> = None;

/// Spell `bare` the way this host names an executable file.
///
/// Callers name the *program*; the host decides whether that program is a file
/// called `bare` or `bare.exe`. Only the file spelling changes here — PATH
/// search order and `PATHEXT` are search concerns, not naming ones.
pub fn file_name(bare: &str) -> String {
    match EXECUTABLE_EXTENSION {
        Some(extension) => format!("{bare}.{extension}"),
        None => bare.to_owned(),
    }
}

/// Path to a sibling program installed beside the running image.
///
/// Returns `None` when the current image cannot be resolved, has no parent
/// directory, or the sibling is not a file — all of which mean the same thing
/// to a caller: this program is not installed next to us, look elsewhere.
pub fn sibling_of_current_image(bare: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let candidate = current.parent()?.join(file_name(bare));
    candidate.is_file().then_some(candidate)
}
