// Phase-1 evidence: the current Command-only late lint accepts all three
// constructs below. Phase 2 converts this into a fail fixture.
#[cfg(windows)]
fn private_windows_only() {}

#[cfg(not(windows))]
use std::os::unix::ffi::OsStrExt as _;

fn main() {
    let _is_windows = cfg!(windows);
}
