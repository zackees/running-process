//! Dev-mode self-relocation ("shadow copy").
//!
//! When the daemon binary lives inside a Cargo `target/` directory it is at
//! risk of being overwritten by subsequent builds while the daemon is running.
//! To guard against this we copy the executable to a stable "shadow" directory
//! and re-exec from there.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// FNV-1a hash (deterministic, no external deps)
// ---------------------------------------------------------------------------

/// 64-bit FNV-1a hash.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Produce a 16-hex-char scope hash for the given working directory.
pub fn scope_hash(cwd: &Path) -> String {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let normalized = canonical.to_string_lossy().to_lowercase();
    format!("{:016x}", fnv1a_64(normalized.as_bytes()))
}

// ---------------------------------------------------------------------------
// Build-output detection
// ---------------------------------------------------------------------------

/// Returns `true` when `exe` looks like it lives inside a Cargo build output
/// directory (`target/debug` or `target/release`).
pub fn is_in_build_output(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("target/debug")
        || s.contains("target\\debug")
        || s.contains("target/release")
        || s.contains("target\\release")
}

// ---------------------------------------------------------------------------
// Shadow directory
// ---------------------------------------------------------------------------

/// Directory for shadow-copied daemon binaries.
///
/// This is the same directory the cleanup verifier inspects, and deliberately
/// the same call: a writer and a checker that derive a path separately will
/// agree right up until they do not. These two had already drifted apart on
/// their fallbacks -- with no `XDG_RUNTIME_DIR` this wrote under
/// `data_local_dir()` while the verifier looked under `/tmp/<product>-<uid>`,
/// and on macOS with no home directory the two `/tmp` shapes differed. The
/// primary paths always agreed, which is why it went unnoticed.
pub fn shadow_dir() -> PathBuf {
    crate::client::paths::shadow_dir_view()
}

// ---------------------------------------------------------------------------
// Self-relocation
// ---------------------------------------------------------------------------

const SHADOW_MARKER_ENV: &str = "RUNNING_PROCESS_DAEMON_SHADOWED";

/// If the current executable lives in a Cargo build output directory, copy it
/// to the shadow directory and re-exec from there.
///
/// Returns:
/// * `Ok(true)` — we spawned / exec'd the shadow copy (caller should exit on
///   Windows; on Unix the process is replaced).
/// * `Ok(false)` — no relocation was needed (already shadowed or not a dev
///   build).
pub fn maybe_self_relocate() -> Result<bool, Box<dyn std::error::Error>> {
    // If we are already the shadow copy, nothing to do.
    if crate::env_vars::DAEMON_SHADOWED.is_set() {
        return Ok(false);
    }

    let current_exe = std::env::current_exe()?;
    if !is_in_build_output(&current_exe) {
        return Ok(false);
    }

    let shadow = shadow_dir();
    std::fs::create_dir_all(&shadow)?;

    let file_name = current_exe
        .file_name()
        .ok_or("current exe has no file name")?;
    let dest = shadow.join(file_name);

    std::fs::copy(&current_exe, &dest)?;

    reexec_from_shadow(&dest)?;
    Ok(true) // unreachable on Unix (exec replaces process)
}

#[cfg(unix)]
fn reexec_from_shadow(exe: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let err = std::process::Command::new(exe)
        .args(&args)
        .env(SHADOW_MARKER_ENV, "1")
        .exec(); // replaces process; only returns on error
    Err(Box::new(err))
}

#[cfg(windows)]
fn reexec_from_shadow(exe: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut command = std::process::Command::new(exe);
    command.args(&args).env(SHADOW_MARKER_ENV, "1");
    crate::spawn_daemon(&mut command)?;
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Stale shadow cleanup
// ---------------------------------------------------------------------------

/// Remove stale shadow copies that are not the current executable.
///
/// This is a best-effort operation — errors are silently ignored.
pub fn cleanup_stale_shadows() {
    let dir = shadow_dir();
    if !dir.exists() {
        return;
    }

    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path != current_exe {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::Mutex;

    /// The daemon writes its shadow copy where the cleanup verifier looks.
    ///
    /// These were separate derivations that agreed on their primary paths and
    /// disagreed on their fallbacks, so nothing caught the drift. Asserting
    /// the two are equal pins the property rather than either answer, and
    /// holds on every host without naming one.
    #[test]
    fn the_shadow_directory_is_the_one_the_verifier_inspects() {
        assert_eq!(shadow_dir(), crate::client::paths::shadow_dir_view());
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_var<T>(
        key: &str,
        value: Option<&OsStr>,
        f: impl FnOnce() -> T + std::panic::UnwindSafe,
    ) -> T {
        let old = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        let result = std::panic::catch_unwind(f);
        match old {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn fnv1a_known_vector() {
        // Empty string should match the well-known FNV-1a offset basis.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn scope_hash_deterministic() {
        let a = scope_hash(Path::new("/tmp/foo"));
        let b = scope_hash(Path::new("/tmp/foo"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16); // 16 hex chars
    }

    #[test]
    fn build_output_detection() {
        assert!(is_in_build_output(Path::new(
            "/home/user/project/target/debug/daemon"
        )));
        assert!(is_in_build_output(Path::new(
            "C:\\dev\\project\\target\\release\\daemon.exe"
        )));
        assert!(!is_in_build_output(Path::new("/usr/local/bin/daemon")));
    }

    #[test]
    fn shadow_dir_is_not_empty() {
        let d = shadow_dir();
        assert!(!d.as_os_str().is_empty());
    }

    #[test]
    fn maybe_self_relocate_skips_when_shadow_marker_is_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        with_env_var(SHADOW_MARKER_ENV, Some(OsStr::new("1")), || {
            assert!(!maybe_self_relocate().expect("shadow marker should skip relocation"));
        });
    }

    #[cfg(target_os = "linux")]
    fn with_temp_shadow_root<T>(f: impl FnOnce(&Path) -> T + std::panic::UnwindSafe) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().as_os_str().to_os_string();

        with_env_var("XDG_RUNTIME_DIR", Some(root.as_os_str()), || f(temp.path()))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shadow_dir_respects_platform_runtime_env() {
        with_temp_shadow_root(|root| {
            let dir = shadow_dir();
            assert!(dir.starts_with(root));
            assert!(dir.ends_with(Path::new("running-process").join("run")));
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_stale_shadows_removes_files_but_leaves_dirs() {
        with_temp_shadow_root(|_| {
            let dir = shadow_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let stale_file = dir.join("old-daemon-copy");
            let nested_dir = dir.join("nested");
            std::fs::write(&stale_file, b"old").unwrap();
            std::fs::create_dir_all(&nested_dir).unwrap();

            cleanup_stale_shadows();

            assert!(!stale_file.exists());
            assert!(nested_dir.exists());
        });
    }
}
