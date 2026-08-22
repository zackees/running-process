//! macOS per-user directory placement for product runtime artifacts.

use std::path::PathBuf;

/// Directory for `product`'s ephemeral runtime artifacts (sockets, pid files).
///
/// macOS ships no `XDG_RUNTIME_DIR`, but honours it when a caller's
/// environment sets one. The fallback qualifies `/tmp` with the caller's uid,
/// because `/tmp` is shared and two accounts must not land on one directory.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join(product);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/{product}-{uid}"))
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
pub fn user_state_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(dir).join(product)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/state").join(product)
    } else {
        PathBuf::from(format!("/tmp/{product}-state"))
    }
}

/// Root under which `product` keeps per-run scratch data.
///
/// macOS gives each user a `Library/Caches` for exactly this class of data,
/// which is where it belongs rather than beside the sockets.
pub fn user_run_data_root(product: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/Caches")
        .join(product)
}
