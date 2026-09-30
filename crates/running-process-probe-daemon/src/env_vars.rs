//! Every environment variable this crate reads in production, declared in one
//! place (#1101).
//!
//! The reading mechanism is `running_process_platform_internal::env`; this is
//! the policy for `running-process-probe-daemon`. Each reader goes through one
//! of these constants rather than a string literal, so an embedder can learn
//! what `rpprobed` reads from [`DECLARED_PROBE_DAEMON`] instead of grepping
//! call sites.
//!
//! A variable is declared by the crate that owns it. The daemon also reads
//! `RUNNING_PROCESS_PROBE_CRASH_DIR`, which the probe crate owns because the
//! crashing process and the daemon must agree on it; the daemon reads
//! `running_process_probe::env_vars::PROBE_CRASH_DIR` rather than repeating it.
//!
//! The readers keep their original parsing: the beacon port is parsed exactly
//! as written, and the bind-all guard opens for `1` and nothing else.

use running_process_platform_internal::env::{EnvKind, Owner};

running_process_platform_internal::declare_env_vars! {
    /// Every environment variable this crate owns and reads in production,
    /// sorted by name; `declarations_are_sorted_unique_and_documented` holds
    /// that.
    pub const DECLARED_PROBE_DAEMON;
    PROBE_BEACON_PORT => "RUNNING_PROCESS_PROBE_BEACON_PORT",
        EnvKind::Number { zero_selects_default: false }, Owner::Crate, "no beacon is served",
        "Loopback port for the probe daemon's single-instance beacon.";
    PROBE_BIND_ALL => "RUNNING_PROCESS_PROBE_BIND_ALL",
        EnvKind::ExactValue("1"), Owner::Crate, "the HTTP surface binds loopback only",
        "Set to exactly `1` to let the probe HTTP surface bind a non-loopback address.";
    PROBE_DISCOVERY => "RUNNING_PROCESS_PROBE_DISCOVERY",
        EnvKind::Path, Owner::Crate, "the standard discovery directory",
        "Directory holding the probe daemon's discovery file, for the CLI to find it.";
    PROBE_WORKER => "RUNNING_PROCESS_PROBE_WORKER",
        EnvKind::Path, Owner::Crate, "the worker beside the daemon executable",
        "Path to the symbolization worker binary.";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_sorted_unique_and_documented() {
        let names: Vec<&str> = DECLARED_PROBE_DAEMON.iter().map(|var| var.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "DECLARED_PROBE_DAEMON must be sorted and unique"
        );
        for var in DECLARED_PROBE_DAEMON {
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
    /// the daemon must not re-declare what a crate below it already owns.
    #[test]
    fn no_name_is_also_declared_by_a_crate_below() {
        let below = running_process_platform_internal::env_vars::DECLARED_PLATFORM
            .iter()
            .chain(running_process_probe::env_vars::DECLARED_PROBE)
            .chain(running_process::env_vars::DECLARED);
        let below: Vec<&str> = below.map(|var| var.name).collect();
        for var in DECLARED_PROBE_DAEMON {
            assert!(
                !below.contains(&var.name),
                "{} is also declared by a crate below the daemon",
                var.name
            );
        }
    }

    /// Production code reaches the environment only through declared
    /// variables. This textual scan sees every `cfg` branch, including the
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
