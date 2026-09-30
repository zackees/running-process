//! Every environment variable the worker reads in production, declared in one
//! place (#1101).
//!
//! The reading mechanism is `running_process_platform_internal::env`, taken
//! with `default-features = false` so the worker stays free of Tokio and of
//! `running-process` itself -- it is the process boundary symbol-file parsers
//! live behind, and must not link what the daemon links. This is the worker's
//! policy: each reader goes through one of these constants rather than a
//! string literal, so [`DECLARED_PROBE_WORKER`] is the complete list.
//!
//! The readers keep their original parsing. The path lists are split with
//! `std::env::split_paths` exactly as before, and [`PROBE_LINE_NUMBERS`] keeps
//! its own "present and not `0`" rule.

use running_process_platform_internal::env::{EnvKind, Owner};

running_process_platform_internal::declare_env_vars! {
    /// Every environment variable the worker reads in production, sorted by
    /// name; `declarations_are_sorted_unique_and_documented` holds that.
    pub const DECLARED_PROBE_WORKER;
    PROBE_BUILD_ID_CACHE => "RUNNING_PROCESS_PROBE_BUILD_ID_CACHE",
        EnvKind::Path, Owner::Crate, "the default per-user build-id cache",
        "Platform path-list of build-id cache roots searched for symbol files.";
    PROBE_LINE_NUMBERS => "RUNNING_PROCESS_PROBE_LINE_NUMBERS",
        EnvKind::Text, Owner::Crate, "only function names are resolved",
        "Present and not `0`: resolve `file:line` for probe frames as well as names.";
    PROBE_SYMBOL_PATH => "RUNNING_PROCESS_PROBE_SYMBOL_PATH",
        EnvKind::Path, Owner::Crate, "no additional symbol stores",
        "Platform path-list of additional local symbol-store roots.";
    PROBE_SYMBOL_SERVERS => "RUNNING_PROCESS_PROBE_SYMBOL_SERVERS",
        EnvKind::Text, Owner::Crate, "no symbol server is contacted",
        "Admin-only, comma-separated HTTP(S) symbol-server base URLs.";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_sorted_unique_and_documented() {
        let names: Vec<&str> = DECLARED_PROBE_WORKER.iter().map(|var| var.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "DECLARED_PROBE_WORKER must be sorted and unique"
        );
        for var in DECLARED_PROBE_WORKER {
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
    /// the worker must not re-declare what platform-internal already owns.
    #[test]
    fn no_name_is_also_declared_by_platform_internal() {
        for var in DECLARED_PROBE_WORKER {
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
