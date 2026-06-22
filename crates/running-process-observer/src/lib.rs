//! Sidecar / file-hook tier for `running-process` — #539 follow-up #551.
//!
//! This crate implements the **fourth column** of #539's per-platform
//! acceptance matrix: streaming file-activity events
//! (`FileOpen`/`FileWrite`/`FileClose`/`FileUnlink`/`FileRename`)
//! intercepted in the target process's address space via library-function
//! detours, complementing the snapshot tier
//! ([`read_process_file_handles`](running_process::observer::read_process_file_handles))
//! that already shipped under #539.
//!
//! ## Architecture (off-by-default opt-in)
//!
//! The injector and per-OS interposer code live in this crate, behind
//! the `embed-helper` Cargo feature, and ship via the bundled
//! `running-process-observer-helper` binary embedded at build time and
//! extracted to a per-user cache directory at first use. The main
//! [`running_process`] crate stays **completely clean of injection
//! symbols** (`CreateRemoteThread`, `dlopen` of interposers, etc.) so
//! that static AV / EDR analysis of consumers that don't opt in sees no
//! hooking surface at all. Precedent: Frida's `frida-helper-{32,64}`,
//! Sysmon, VS Profiler.
//!
//! Per-OS injection vehicles land in slices 4–6 of #551:
//!
//! - Windows: DLL injection + `retour-rs` function detours.
//! - Linux: `LD_PRELOAD` of a shared object that shadows libc symbols
//!   via `dlsym(RTLD_NEXT, ...)`. Env-var propagation through `execve()`
//!   re-injects descendants for free.
//! - macOS: `DYLD_INSERT_LIBRARIES` — same shape as `LD_PRELOAD`, with
//!   SIP/hardened-runtime caveats documented per-call-site.
//!
//! ## Slice 1 scope (this scaffold)
//!
//! - [`HookConfig`] type for caller opt-in (always exists; off by
//!   default if `embed-helper` is disabled).
//! - [`HookCapability`] negotiation that reports honestly whether the
//!   embedded helper is available for this build (`feature_enabled`)
//!   and whether the host has the per-OS injection vehicle (filled in
//!   by slices 4–6).
//! - Placeholder `running-process-observer-helper` binary that prints a
//!   version banner and exits — proves the workspace plumbing.
//!
//! No injection, no IPC, no events. Slice 2 adds the embed-and-extract
//! machinery; slice 3 adds the IPC event stream; slices 4–6 add the
//! actual interposer payloads.

// `deny(unsafe_code)` rather than `forbid` so the slice 6d Windows
// injection vehicle in [`inject_windows`] can opt into unsafe via
// `#[allow(unsafe_code)]` on the module. The rest of the crate
// remains unsafe-free.
#![deny(unsafe_code)]
#![warn(missing_docs)]

/// Opt-in configuration that turns the file-hook tier on for a single
/// spawned process (and its descendants, on Linux + macOS where env-var
/// inheritance handles re-injection automatically; Windows uses
/// per-process injection — see slice 6 of #551).
///
/// Constructing a config does not by itself install any hooks. The
/// caller still has to attach it to a process via the integration
/// glue landing in slice 2 (`NativeProcess::with_observer_hooks`).
///
/// With the `embed-helper` feature off, this type still exists but
/// every method that would extract the helper sidecar returns
/// [`HookSupport::FeatureDisabled`]. Lets downstream consumers code
/// against the stable surface regardless of build flags.
#[derive(Debug, Clone, Default)]
pub struct HookConfig {
    _private: (),
}

impl HookConfig {
    /// Construct a default hook config — installs the standard file-IO
    /// hook set when attached to a process. Slices 4–6 of #551 define
    /// the per-OS standard hook set; slice 2 wires this into a spawn.
    pub fn standard() -> Self {
        Self { _private: () }
    }
}

/// Per-OS support level the hook tier reports back to consumers.
///
/// Mirrors the `CapabilitySupport` shape in
/// [`running_process::observer`] but specialized for the
/// hook-feature-flag distinction — knowing the feature is *enabled in
/// this build* is orthogonal to knowing the host kernel supports the
/// per-OS injection vehicle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookSupport {
    /// The `embed-helper` Cargo feature was off at build time. No
    /// sidecar binary is embedded; the consumer must rebuild with the
    /// feature on to use hooks. This is the default for the published
    /// crate so static AV exposure stays at zero.
    FeatureDisabled,
    /// The feature is enabled and the per-OS injection vehicle is
    /// available on this host. Hooks will install on attach.
    Available,
    /// The feature is enabled but the per-OS injection vehicle is
    /// unavailable (e.g. macOS SIP-protected target binary,
    /// hardened-runtime without the required entitlement, or a
    /// platform without an injector implementation yet). Carries a
    /// stable lowercase reason string so consumers can surface it.
    Unavailable {
        /// Why the injection vehicle isn't available right now.
        reason: &'static str,
    },
}

impl HookSupport {
    /// Stable lowercase short name for serialization / matrix
    /// rendering — matches the `as_str()` convention in
    /// [`running_process::observer`].
    pub fn as_str(self) -> &'static str {
        match self {
            HookSupport::FeatureDisabled => "feature-disabled",
            HookSupport::Available => "available",
            HookSupport::Unavailable { .. } => "unavailable",
        }
    }
}

/// Negotiate the hook tier's per-OS support level on this host with
/// this build.
///
/// Phase 1 (slice 1 of #551) always returns
/// [`HookSupport::FeatureDisabled`] because no per-OS injector has
/// landed yet — the feature flag *would* gate them when they do.
/// Slices 4–6 flip each per-OS branch to `Available` /
/// `Unavailable { reason }` honestly.
pub fn negotiate_hook_support() -> HookSupport {
    #[cfg(not(feature = "embed-helper"))]
    {
        HookSupport::FeatureDisabled
    }
    #[cfg(feature = "embed-helper")]
    {
        #[cfg(target_os = "windows")]
        {
            // Slice 6d landed: the `inject_into_pid` vehicle is wired.
            // The interposer DLL itself (running-process-observer-
            // interposer-windows) ships separately and the caller
            // provides its on-disk path.
            HookSupport::Available
        }
        // Linux + macOS injectors land alongside their slice 7
        // integration tests; until then honestly report that the
        // feature is on but no injector has been wired for this OS.
        #[cfg(not(target_os = "windows"))]
        {
            HookSupport::Unavailable {
                reason: "#551: per-OS injector not yet wired (slices 4–6 pending)",
            }
        }
    }
}

/// Cache + extract machinery for the embedded helper binary
/// ([#551 slice 2](https://github.com/zackees/running-process/issues/551)).
///
/// Only compiled when the `embed-helper` feature is enabled — the
/// off-by-default path keeps consumers' binaries free of the
/// `dirs` + `blake3` transitive deps and the helper-extraction code
/// path entirely.
///
/// API summary:
///
/// - [`helper_cache_dir`] — per-OS cache directory the helper lives in
///   (XDG cache on Linux, `~/Library/Caches` on macOS,
///   `%LOCALAPPDATA%` on Windows, via the `dirs` crate).
/// - [`helper_filename`] — stable per-build filename
///   (`running-process-observer-helper-<version>-<target>.[exe]`).
/// - [`extract_helper_blob`] — idempotent: writes `blob` to
///   `<cache>/<filename>` if the existing file's blake3 hash doesn't
///   match, sets the executable bit on Unix. Returns the path.
///
/// The helper binary's bytes come from the consumer of this crate
/// today — typically a future `include_bytes!` site that slice 2b
/// will introduce once the bin-as-build-dep chain is wired. Keeping
/// the function generic over the blob lets slice 2 ship the cache
/// half independently.
#[cfg(feature = "embed-helper")]
pub mod embed {
    use std::io;
    use std::path::PathBuf;

    /// Top-level subdirectory under the OS cache root that holds the
    /// extracted helper. Versioned via the crate's package version so
    /// stale helpers from older installs don't get reused.
    const CACHE_SUBDIR: &str = "running-process-observer";

    /// Return the OS-specific cache directory the helper lives in,
    /// creating it on disk if it doesn't already exist.
    ///
    /// Paths:
    /// - **Linux**: `$XDG_CACHE_HOME/running-process-observer` or
    ///   `~/.cache/running-process-observer`
    /// - **macOS**: `~/Library/Caches/running-process-observer`
    /// - **Windows**: `%LOCALAPPDATA%\running-process-observer`
    pub fn helper_cache_dir() -> io::Result<PathBuf> {
        let base = dirs::cache_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine OS cache directory via dirs::cache_dir()",
            )
        })?;
        let dir = base.join(CACHE_SUBDIR);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Stable filename for the helper binary, derived from the crate's
    /// package version and the build target triple. Different versions
    /// or targets get separate filenames so multiple installs can
    /// coexist on the same machine.
    pub fn helper_filename() -> String {
        let version = env!("CARGO_PKG_VERSION");
        let target = std::env::consts::ARCH;
        let os = std::env::consts::OS;
        let ext = if cfg!(windows) { ".exe" } else { "" };
        format!("running-process-observer-helper-{version}-{target}-{os}{ext}")
    }

    /// The fully-resolved path the extracted helper will live at.
    /// Combines [`helper_cache_dir`] + [`helper_filename`].
    pub fn helper_cache_path() -> io::Result<PathBuf> {
        Ok(helper_cache_dir()?.join(helper_filename()))
    }

    /// Extract `blob` (the helper binary's raw bytes — typically
    /// sourced from an `include_bytes!` site at the consumer) to the
    /// crate's standard cache path ([`helper_cache_path`]).
    ///
    /// Thin wrapper around [`extract_helper_blob_to`]. Tests should
    /// prefer the explicit-path variant to avoid racing the shared
    /// cache.
    pub fn extract_helper_blob(blob: &[u8]) -> io::Result<PathBuf> {
        let path = helper_cache_path()?;
        extract_helper_blob_to(&path, blob)
    }

    /// Extract `blob` to a caller-supplied destination path.
    /// Idempotent: if the existing file's blake3 hash matches, no
    /// write happens. On Unix the resulting file gets `0o755`
    /// permissions; on Windows extensions are sufficient.
    ///
    /// Returns the path on success (== `path`).
    pub fn extract_helper_blob_to(path: &std::path::Path, blob: &[u8]) -> io::Result<PathBuf> {
        let expected_hash = blake3::hash(blob);
        if path.exists() {
            if let Ok(existing) = std::fs::read(path) {
                if blake3::hash(&existing) == expected_hash {
                    return Ok(path.to_path_buf());
                }
            }
            // Mismatch (or read error). Fall through and re-write.
        }
        // Atomic-ish write: write to a sibling temp file then rename.
        // Use a per-process suffix so two extractions in flight at
        // the same time don't clobber each other's partial.
        let tmp = path.with_extension(format!("partial.{}", std::process::id()));
        std::fs::write(&tmp, blob)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(path.to_path_buf())
    }
}

/// Windows DLL-injection vehicle ([#551 slice 6d]). Drives
/// `CreateRemoteThread(LoadLibraryW, dll_path)` against a target
/// PID to load the interposer DLL into its address space.
///
/// Gated on `feature = "embed-helper"` + `target_os = "windows"` so
/// non-Windows builds and feature-off builds pay zero static-AV
/// exposure cost.
///
/// [#551 slice 6d]: https://github.com/zackees/running-process/issues/551
#[cfg(all(feature = "embed-helper", target_os = "windows"))]
pub mod inject_windows;

#[cfg(all(feature = "embed-helper", target_os = "windows"))]
pub use inject_windows::inject_into_pid;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_support_string_forms_are_stable() {
        assert_eq!(HookSupport::FeatureDisabled.as_str(), "feature-disabled");
        assert_eq!(HookSupport::Available.as_str(), "available");
        assert_eq!(
            HookSupport::Unavailable {
                reason: "test reason"
            }
            .as_str(),
            "unavailable"
        );
    }

    #[test]
    fn negotiate_default_build_reports_feature_disabled() {
        // The published crate ships with `embed-helper` off so consumers
        // pay zero static-AV exposure cost unless they explicitly opt
        // in. Lock that contract.
        #[cfg(not(feature = "embed-helper"))]
        {
            assert_eq!(negotiate_hook_support(), HookSupport::FeatureDisabled);
        }
        #[cfg(feature = "embed-helper")]
        {
            let s = negotiate_hook_support();
            // Slice 6d landed the Windows injector — on Windows the
            // honest answer is now Available. Linux + macOS keep
            // reporting Unavailable with a slice pointer until their
            // injectors land (separate follow-up slices).
            #[cfg(target_os = "windows")]
            assert_eq!(s, HookSupport::Available);
            #[cfg(not(target_os = "windows"))]
            assert!(matches!(s, HookSupport::Unavailable { reason } if reason.contains("#551")));
        }
    }

    #[test]
    fn standard_hook_config_constructs() {
        let _ = HookConfig::standard();
    }

    #[cfg(feature = "embed-helper")]
    mod embed_tests {
        use super::super::embed::*;

        #[test]
        fn helper_cache_dir_creates_and_returns_a_path() {
            let p = helper_cache_dir().expect("cache dir");
            assert!(
                p.exists() && p.is_dir(),
                "expected cache dir to exist, got {p:?}"
            );
            // The trailing component must be our versioned subdir.
            assert!(
                p.ends_with("running-process-observer"),
                "expected cache path to end in running-process-observer, got {p:?}"
            );
        }

        #[test]
        fn helper_filename_carries_version_and_arch() {
            let name = helper_filename();
            assert!(name.starts_with("running-process-observer-helper-"));
            assert!(
                name.contains(env!("CARGO_PKG_VERSION")),
                "filename must carry the crate version: {name}"
            );
            #[cfg(windows)]
            assert!(name.ends_with(".exe"), "Windows filename needs .exe: {name}");
            #[cfg(not(windows))]
            assert!(!name.contains(".exe"), "Unix filename must not have .exe: {name}");
        }

        #[test]
        fn extract_helper_blob_writes_and_is_idempotent() {
            // Use a per-test tempdir so parallel tests don't race on
            // the shared `helper_cache_dir()`. The high-level
            // `extract_helper_blob()` wrapper is exercised separately
            // by the smoke test below.
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("helper-bin");
            let blob: &[u8] = b"#!/bin/sh\necho stub helper bytes\n";
            let p1 = extract_helper_blob_to(&path, blob).expect("first extract");
            assert!(p1.exists(), "extracted file should exist at {p1:?}");
            let read1 = std::fs::read(&p1).expect("read back");
            assert_eq!(read1, blob, "extracted bytes must match input");

            // Second extract with identical bytes is a no-op (hash
            // matches), returns the same path.
            let p2 = extract_helper_blob_to(&path, blob).expect("second extract");
            assert_eq!(p1, p2, "idempotent re-extract should return same path");

            // Third extract with DIFFERENT bytes should rewrite.
            let blob2: &[u8] = b"#!/bin/sh\necho different stub\n";
            let p3 = extract_helper_blob_to(&path, blob2).expect("third extract");
            assert_eq!(p1, p3);
            let read3 = std::fs::read(&p3).expect("read back v2");
            assert_eq!(read3, blob2, "rewrite must replace contents");
        }

        #[cfg(unix)]
        #[test]
        fn extract_helper_blob_sets_executable_bit_on_unix() {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("helper-bin");
            let blob: &[u8] = b"#!/bin/sh\nexit 0\n";
            let p = extract_helper_blob_to(&path, blob).expect("extract");
            let mode = std::fs::metadata(&p).expect("stat").permissions().mode();
            // Owner exec bit must be set.
            assert_ne!(mode & 0o100, 0, "owner exec bit missing: mode=0o{:o}", mode);
        }

        #[test]
        fn extract_helper_blob_smoke_test_against_real_cache_dir() {
            // Exercise the wrapper that targets the actual cache
            // directory once, to keep the cache-path resolution path
            // in test coverage. Uses a distinctive blob so a
            // concurrent test sharing the same path (shouldn't
            // happen — only one test calls this) would be diagnosable.
            let blob: &[u8] = b"smoke-test-distinctive-blob-marker\n";
            let p = extract_helper_blob(blob).expect("smoke extract");
            assert!(p.exists());
            let read_back = std::fs::read(&p).expect("read");
            assert_eq!(read_back, blob);
            // Cleanup so we don't pollute the user's cache dir long-term.
            let _ = std::fs::remove_file(&p);
        }
    }
}
