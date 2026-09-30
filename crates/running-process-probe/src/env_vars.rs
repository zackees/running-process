//! Every environment variable this crate reads in production, declared in one
//! place (#1101).
//!
//! The reading mechanism is `running_process_platform_internal::env`; this is
//! the policy for `running-process-probe`. Each reader goes through one of
//! these constants rather than a string literal, so an embedder can learn what
//! the probe reads from [`DECLARED_PROBE`] instead of grepping call sites.
//!
//! A variable is declared by the crate that owns it. The probe also reads
//! `XDG_RUNTIME_DIR`, which `running-process-platform-internal` already
//! declares; it reads that declaration rather than repeating it here.
//!
//! The readers keep their original parsing -- these switches predate the
//! shared parsers, and moving a read onto a table is not a reason to change
//! what an existing value means. So [`NO_CRASH_HANDLER`] is still read with
//! its own `1`/`true`/`yes` rule, and the directory overrides still treat an
//! empty value as set.

use running_process_platform_internal::env::{EnvKind, Owner};

running_process_platform_internal::declare_env_vars! {
    /// Every environment variable this crate owns and reads in production,
    /// sorted by name; `declarations_are_sorted_unique_and_documented` holds
    /// that.
    pub const DECLARED_PROBE;
    INJECT_WAIT_TIMEOUT_MS => "RUNNING_PROCESS_INJECT_WAIT_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "thirty seconds",
        "How long Windows DLL injection waits for the remote loader thread, in milliseconds.";
    PROBE_CRASH_DIR => "RUNNING_PROCESS_PROBE_CRASH_DIR",
        EnvKind::Path, Owner::Crate, "a directory under the owner-private runtime root",
        "Where durable crash reports are written.";
    PROBE_NO_CRASH_HANDLER => "RUNNING_PROCESS_PROBE_NO_CRASH_HANDLER",
        EnvKind::Text, Owner::Crate, "the native crash handler is installed",
        "Set to `1`, `true` or `yes` to leave native crash handlers untouched.";
    PROBE_SPOOL_DIR => "RUNNING_PROCESS_PROBE_SPOOL_DIR",
        EnvKind::Path, Owner::Crate, "a directory under the owner-private runtime root",
        "Where pending crash records are spooled before the daemon collects them.";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_sorted_unique_and_documented() {
        let names: Vec<&str> = DECLARED_PROBE.iter().map(|var| var.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "DECLARED_PROBE must be sorted and unique");
        for var in DECLARED_PROBE {
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

    /// A name is declared once across the crates that share the mechanism:
    /// the probe must not re-declare what platform-internal already owns.
    #[test]
    fn no_name_is_also_declared_by_platform_internal() {
        for var in DECLARED_PROBE {
            assert!(
                !running_process_platform_internal::env_vars::DECLARED_PLATFORM
                    .iter()
                    .any(|other| other.name == var.name),
                "{} is declared by both crates",
                var.name
            );
        }
    }

    /// Production code reaches the environment only through declared
    /// variables. This textual scan sees every `cfg` branch, including the
    /// Windows-only injection path a Linux Dylint run cannot.
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
