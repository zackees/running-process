//! Every environment variable this crate reads in production, declared in one
//! place (#1101).
//!
//! [`crate::env`] is the mechanism; this is the policy for
//! `running-process-platform-internal`. Each reader goes through one of these
//! constants rather than a string literal, so an embedder can learn what the
//! crate reads from [`DECLARED_PLATFORM`] instead of grepping call sites.
//!
//! Many of these are read with [`EnvVar::os`](crate::env::EnvVar::os) rather
//! than [`EnvVar::path`](crate::env::EnvVar::path): the original readers
//! treated `VAR=` (empty) as set, and moving them onto a table is not a reason
//! to change what they do.
//!
//! # One owner per name
//!
//! Some of these are also read by `running-process`. Each name is declared
//! once, here, by the lowest crate that reads it; `running_process::env_vars`
//! refers to the same constant rather than repeating it, and its
//! `all_declared()` is the one combined inventory. The shared declarations
//! keep the wording `running_process::env_vars::DECLARED` has always
//! published, so that table did not change when they moved.
//!
//! Not covered, deliberately:
//! - Reads keyed by a caller-supplied name (`ipc` descriptor keys on Linux and
//!   macOS). The name is data there, not a variable this crate chooses, so
//!   there is nothing to declare; they go through
//!   [`os_named`](crate::env::os_named).
//! - Save/restore and fixture plumbing inside `#[cfg(test)]` code.

use crate::env::{EnvKind, Owner};

crate::declare_env_vars! {
    /// Every environment variable this crate reads in production, sorted by
    /// name; `declarations_are_sorted_unique_and_documented` holds that.
    pub const DECLARED_PLATFORM;
    COMPUTERNAME => "COMPUTERNAME",
        EnvKind::Text, Owner::Foreign, "hostname is unknown",
        "Windows machine name, reported as the host name.";
    DISPLAY => "DISPLAY",
        EnvKind::Text, Owner::Foreign, "no X11 display; window icons unsupported",
        "X11 display; its presence is what makes a Linux window icon possible.";
    HOME => "HOME",
        EnvKind::Path, Owner::Foreign, "autostart paths cannot be resolved",
        "Home directory; roots the autostart entries and the fallback APE loader cache directory.";
    LOCALAPPDATA => "LOCALAPPDATA",
        EnvKind::Path, Owner::Foreign, "the platform default is derived",
        "Windows per-user application data root.";
    PATH => "PATH",
        EnvKind::Text, Owner::Foreign, "the child inherits no explicit PATH",
        "Executable search path, forwarded to the symbolization worker and searched for an APE image or loader.";
    APE_CACHE_DIR => "RUNNING_PROCESS_APE_CACHE_DIR",
        EnvKind::Path, Owner::Crate, "the XDG cache, runtime and temporary directories",
        "Preferred directory for loaders extracted from APE images; used only when private and exec-capable.";
    APE_LOADER => "RUNNING_PROCESS_APE_LOADER",
        EnvKind::Path, Owner::Crate, "the image's embedded loader, then `ape`, then `/bin/sh`",
        "Explicit loader (an `ape` binary or a POSIX shell) for APE images.";
    CONPTY_CACHE => "RUNNING_PROCESS_CONPTY_CACHE",
        EnvKind::Path, Owner::Crate, "the platform cache directory",
        "Root under which the ConPTY sidecar is cached on Windows.";
    CONPTY_DIAGNOSTICS => "RUNNING_PROCESS_CONPTY_DIAGNOSTICS",
        EnvKind::Text, Owner::Crate, "ConPTY resolution is silent",
        "Print how the ConPTY implementation was chosen and fetched, to stderr.";
    CONPTY_OFFLINE => "RUNNING_PROCESS_CONPTY_OFFLINE",
        EnvKind::Text, Owner::Crate, "the ConPTY sidecar may be fetched",
        "Forbid fetching the ConPTY sidecar over the network.";
    CONPTY_SIDECAR_FETCH_TIMEOUT_MS => "RUNNING_PROCESS_CONPTY_SIDECAR_FETCH_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in fetch timeout",
        "Upper bound on the ConPTY sidecar download, in milliseconds.";
    KILL_DRAIN_TIMEOUT_MS => "RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: false }, Owner::Crate, "two seconds",
        "How long `kill()` waits for output capture to drain, in milliseconds.";
    NATIVE_TERMINAL_INPUT_TRACE_PATH => "RUNNING_PROCESS_NATIVE_TERMINAL_INPUT_TRACE_PATH",
        EnvKind::Text, Owner::Crate, "terminal input is not traced",
        "Where Windows native terminal input events are traced.";
    USE_SYSTEM_CONPTY => "RUNNING_PROCESS_USE_SYSTEM_CONPTY",
        EnvKind::Text, Owner::Crate, "the bundled ConPTY sidecar is preferred",
        "Use the system ConPTY instead of the bundled sidecar on Windows.";
    TMPDIR => "TMPDIR",
        EnvKind::Path, Owner::Foreign, "the platform temporary directory",
        "macOS per-session temporary directory; a broker endpoint root.";
    WAYLAND_DISPLAY => "WAYLAND_DISPLAY",
        EnvKind::Text, Owner::Foreign, "not a Wayland session",
        "Wayland session marker; window icons are unsupported under Wayland.";
    WINDOWID => "WINDOWID",
        EnvKind::Text, Owner::Foreign, "the X11 window is unknown",
        "X11 window id exported by the terminal emulator.";
    WT_SESSION => "WT_SESSION",
        EnvKind::Text, Owner::Foreign, "not inside Windows Terminal",
        "Windows Terminal session marker; runtime window icons are degraded there.";
    XDG_CACHE_HOME => "XDG_CACHE_HOME",
        EnvKind::Path, Owner::Foreign, "`~/.cache` is used",
        "XDG per-user cache root; where extracted APE loaders are installed.";
    XDG_CONFIG_HOME => "XDG_CONFIG_HOME",
        EnvKind::Path, Owner::Foreign, "`~/.config` is used",
        "XDG per-user configuration root; where service definitions are read.";
    XDG_DATA_HOME => "XDG_DATA_HOME",
        EnvKind::Path, Owner::Foreign, "the platform default is derived",
        "XDG per-user data root, used by the daemon runtime collector.";
    XDG_RUNTIME_DIR => "XDG_RUNTIME_DIR",
        EnvKind::Path, Owner::Foreign, "a per-user directory under /tmp",
        "XDG per-user runtime root; where broker sockets and fallback APE loaders are placed.";
    XDG_STATE_HOME => "XDG_STATE_HOME",
        EnvKind::Path, Owner::Foreign, "~/.local/state",
        "XDG state root.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Owner;

    #[test]
    fn declarations_are_sorted_unique_and_documented() {
        let names: Vec<&str> = DECLARED_PLATFORM.iter().map(|var| var.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "DECLARED_PLATFORM must be sorted and unique");
        for var in DECLARED_PLATFORM {
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
        }
    }

    #[test]
    fn owner_follows_the_name() {
        for var in DECLARED_PLATFORM {
            let ours = var.name.starts_with("RUNNING_PROCESS_");
            assert_eq!(
                var.owner == Owner::Crate,
                ours,
                "{} is declared {:?}",
                var.name,
                var.owner
            );
        }
    }

    /// Direct calls exempted by their argument text. None remain: reads keyed
    /// by a caller-supplied name go through [`crate::env::os_named`] instead,
    /// which keeps the exemption out of this list and inside the mechanism.
    const DYNAMIC_KEYS: &[&str] = &[];

    /// Production code reaches the environment only through [`crate::env`]:
    /// any direct `std::env` variable call outside test code is drift. The
    /// name keeps its history; there are no dynamic-key exemptions left (see
    /// [`DYNAMIC_KEYS`]).
    ///
    /// Test code is recognised by layout: a `#[cfg(test)]` (or
    /// `#[cfg(all(test, ..))]`) inline `mod name {` runs to the end of its file
    /// in this crate, as do `*_tests.rs` files and `tests/` directories.
    #[test]
    fn production_code_calls_std_env_only_for_dynamic_keys() {
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
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                visit(&path, offenders);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs")
                || path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name == "env.rs" || name == "env_vars.rs" || name.ends_with("_tests.rs")
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
                .find_map(|call| line.find(call).map(|at| &line[at + call.len()..]));
                if let Some(arg) = direct {
                    if !DYNAMIC_KEYS.iter().any(|key| arg.starts_with(key)) {
                        offenders.push(format!("{}:{}", path.display(), index + 1));
                    }
                }
            }
        }
    }
}
