//! Every environment variable the Python extension reads in production,
//! declared in one place (#1101).
//!
//! The mechanism is `running_process_platform_internal::env`, reached through
//! `running_process::env_vars`. Variables a lower crate already declares --
//! `RUNNING_PROCESS_NO_TRACKING`, `LOCALAPPDATA`, `XDG_STATE_HOME`, `HOME` --
//! are read through those declarations rather than repeated here; this table
//! holds only what the extension alone owns.
//!
//! The readers keep their original parsing: `PID_DB` is still trimmed and
//! ignored when blank, and `NO_TRACKING` is still on for `1` or `true` only.

use running_process::env_vars::{EnvKind, Owner};

running_process::env_vars::declare_env_vars! {
    /// Every environment variable the extension alone owns and reads in
    /// production, sorted by name. Read by the drift tests; the extension
    /// itself reads the constants.
    #[allow(dead_code)]
    pub const DECLARED_PY;
    PID_DB => "RUNNING_PROCESS_PID_DB",
        EnvKind::Path, Owner::Crate, "a database under the per-user state directory",
        "Path of the tracked-process database used for orphan cleanup.";
}

/// Set (or unset) `key` for the duration of `f`, restoring it afterwards, with
/// every such test serialised on one lock.
///
/// Test-only save/restore lives here, beside the declarations, so production
/// modules contain no direct environment call at all.
#[cfg(test)]
pub(crate) fn with_locked_env_var<T>(
    key: &'static str,
    value: Option<&str>,
    f: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> T {
    let _guard = crate::helpers::test_env_lock().lock().unwrap();
    let previous = std::env::var_os(key);
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }

    let result = std::panic::catch_unwind(f);

    match previous {
        Some(previous) => std::env::set_var(key, previous),
        None => std::env::remove_var(key),
    }

    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_sorted_unique_and_documented() {
        let names: Vec<&str> = DECLARED_PY.iter().map(|var| var.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "DECLARED_PY must be sorted and unique");
        for var in DECLARED_PY {
            assert!(
                !var.summary.trim().is_empty(),
                "{} has no summary",
                var.name
            );
            assert!(
                !var.default.trim().is_empty(),
                "{} has no default",
                var.name
            );
            assert_eq!(
                var.owner == Owner::Crate,
                var.name.starts_with("RUNNING_PROCESS_"),
                "{} is declared {:?}",
                var.name,
                var.owner
            );
        }
    }

    /// A name is declared once: the extension must not re-declare anything
    /// `running-process` or the platform layer already owns.
    #[test]
    fn no_name_is_also_declared_by_a_crate_below() {
        let below = running_process::env_vars::all_declared();
        for var in DECLARED_PY {
            assert!(
                !below.iter().any(|other| other.name == var.name),
                "{} is also declared by a crate below the extension",
                var.name
            );
        }
    }

    /// Production code reaches the environment only through declared
    /// variables. This textual scan sees every `cfg` branch, including
    /// platform-specific paths a Linux Dylint run cannot.
    ///
    /// Test code is recognised by layout: a `#[cfg(test)]` (or
    /// `#[cfg(all(test, ..))]`) inline `mod name {` runs to the end of its
    /// file in this crate, as do `tests.rs`/`*_tests.rs` files and `tests/`
    /// directories.
    #[test]
    fn production_code_calls_std_env_only_through_declarations() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(&root, &mut offenders);
        assert!(
            offenders.is_empty(),
            "undeclared environment reads: {offenders:#?}"
        );
    }

    fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    visit(&path, offenders);
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs")
                || path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name == "env_vars.rs" || name == "tests.rs" || name.ends_with("_tests.rs")
                })
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            let lines: Vec<&str> = text.lines().collect();
            let cut = lines
                .iter()
                .enumerate()
                .position(|(index, line)| {
                    let line = line.trim_start();
                    (line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test"))
                        && lines[index + 1..]
                            .iter()
                            .map(|next| next.trim())
                            .find(|next| !next.is_empty() && !next.starts_with("#["))
                            .is_some_and(|next| next.starts_with("mod ") && next.ends_with('{'))
                })
                .unwrap_or(lines.len());
            for (index, line) in lines[..cut].iter().enumerate() {
                let direct = [
                    "env::var(",
                    "env::var_os(",
                    "env::set_var(",
                    "env::remove_var(",
                ]
                .iter()
                .any(|call| line.contains(call));
                if direct {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
}
