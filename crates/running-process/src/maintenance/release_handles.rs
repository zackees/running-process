//! `running-process maintenance release-handles --path <PATH>`.
//!
//! Phase 1 of #228 (issue #230). The goal is to make
//! `rm -rf <PATH>` reliable on Windows even when a daemon process is
//! holding handles inside `<PATH>` — see soldr#710.
//!
//! ## Per-platform behaviour
//!
//! - **POSIX**: no-op. Linux/macOS use delete-on-close semantics, so
//!   `rm -rf` always succeeds; the subcommand exits 0 with an
//!   informational message.
//! - **Windows**: scaffold-only in Phase 1. The full handler depends
//!   on the manifest registry that ships in Phase 2 (#231). Until
//!   then the subcommand returns a successful "no manifests to scan
//!   yet" result so downstream callers (clud-pr, soldr cleanup) can
//!   start wiring the call site without changing their exit-code
//!   handling later.
//!
//! ## Why ship the POSIX no-op now?
//!
//! Cross-platform tooling (soldr's clud-pr workflow, CI helpers) can
//! call the subcommand unconditionally and get the right behaviour on
//! every host. Without the no-op surface, every caller would need its
//! own `cfg(unix)` short-circuit.

use std::path::{Path, PathBuf};

/// Errors emitted by [`run_release_handles`].
#[derive(Debug, thiserror::Error)]
pub enum ReleaseHandlesError {
    /// The supplied `--path` argument was empty.
    #[error("--path must be non-empty")]
    EmptyPath,
}

/// Result of one `release-handles` invocation.
///
/// Stable across Phase 1 → Phase 2 — Phase 2 will populate the
/// `manifests_scanned` / `handles_released` counters that are zero in
/// the Phase 1 stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseHandlesOutcome {
    /// The path the caller asked us to free up.
    pub path: PathBuf,
    /// Informational human-readable message — printed verbatim by the
    /// CLI when `--json` is not set.
    pub message: String,
    /// Number of manifests we walked. Always 0 in Phase 1.
    pub manifests_scanned: u32,
    /// Number of handle-drop requests we issued. Always 0 in Phase 1.
    pub handles_released: u32,
    /// `true` when no further action is needed (POSIX always returns
    /// `true`; Windows-Phase-1 returns `true` because the manifest
    /// registry doesn't exist yet).
    pub already_clean: bool,
}

impl ReleaseHandlesOutcome {
    /// Render as a JSON object. Stable shape across phases — Phase 2
    /// adds counter values but never adds or removes top-level keys.
    pub fn to_json(&self) -> String {
        // Hand-roll the JSON to avoid pulling serde_json into a code
        // path that needs to be cross-platform clean. Field order is
        // chosen for grep-ability.
        format!(
            "{{\
\"path\":\"{path}\",\
\"manifests_scanned\":{manifests},\
\"handles_released\":{handles},\
\"already_clean\":{clean},\
\"message\":\"{message}\"\
}}",
            path = json_escape(&self.path.to_string_lossy()),
            manifests = self.manifests_scanned,
            handles = self.handles_released,
            clean = self.already_clean,
            message = json_escape(&self.message),
        )
    }
}

/// Run the `release-handles` subcommand. Cross-platform entrypoint
/// called by `runpm maintenance release-handles`.
pub fn run_release_handles(path: &Path) -> Result<ReleaseHandlesOutcome, ReleaseHandlesError> {
    let path_str = path.to_string_lossy();
    if path_str.trim().is_empty() {
        return Err(ReleaseHandlesError::EmptyPath);
    }

    #[cfg(unix)]
    {
        Ok(ReleaseHandlesOutcome {
            path: path.to_path_buf(),
            message: format!(
                "POSIX delete-on-close semantics make this a no-op; proceed with `rm -rf {path_str}`"
            ),
            manifests_scanned: 0,
            handles_released: 0,
            already_clean: true,
        })
    }

    #[cfg(windows)]
    {
        // Phase 1 stub. Phase 2 (#231) ships the manifest registry
        // under `%LOCALAPPDATA%\running-process\manifests\` and the
        // full handler will enumerate that directory + send
        // `MaintenanceRequest::ReleaseHandles { path_prefix }` over
        // each live daemon's pipe. For now we return a successful
        // "nothing to do" result so callers can wire the integration
        // unconditionally.
        Ok(ReleaseHandlesOutcome {
            path: path.to_path_buf(),
            message: format!(
                "Phase 2 manifest registry not yet shipped; no daemons to query for handles under \
                 {path_str}. Proceed with rm -rf and report soldr#710 reproductions if encountered."
            ),
            manifests_scanned: 0,
            handles_released: 0,
            already_clean: true,
        })
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_returns_error() {
        let err = run_release_handles(Path::new("")).unwrap_err();
        match err {
            ReleaseHandlesError::EmptyPath => {}
        }
    }

    #[test]
    fn non_empty_path_returns_ok() {
        let outcome = run_release_handles(Path::new("/tmp/example")).expect("ok");
        assert_eq!(outcome.path, PathBuf::from("/tmp/example"));
        assert!(outcome.already_clean);
        assert_eq!(outcome.manifests_scanned, 0);
        assert_eq!(outcome.handles_released, 0);
    }

    #[test]
    fn json_output_has_stable_keys() {
        let outcome = run_release_handles(Path::new("/tmp/example")).expect("ok");
        let json = outcome.to_json();
        assert!(json.contains("\"path\":"));
        assert!(json.contains("\"manifests_scanned\":"));
        assert!(json.contains("\"handles_released\":"));
        assert!(json.contains("\"already_clean\":"));
        assert!(json.contains("\"message\":"));
    }
}
