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

#![forbid(unsafe_code)]
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
        // Slices 4 (Linux), 5 (macOS), 6 (Windows) of #551 flip the
        // appropriate `cfg(target_os = "...")` branch here from
        // FeatureDisabled to Available. Until each slice lands, the
        // honest answer is the feature is on but no injector has been
        // wired for this OS yet.
        HookSupport::Unavailable {
            reason: "#551: per-OS injector not yet wired (slices 4–6 pending)",
        }
    }
}

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
            // With the feature on, slice 1 reports Unavailable + a
            // pointer at the pending slices.
            let s = negotiate_hook_support();
            assert!(matches!(s, HookSupport::Unavailable { reason } if reason.contains("#551")));
        }
    }

    #[test]
    fn standard_hook_config_constructs() {
        let _ = HookConfig::standard();
    }
}
