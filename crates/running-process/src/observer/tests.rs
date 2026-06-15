//! Phase 1 tests for #221. Deterministic, fast, and OS-agnostic: they
//! drive a short-lived child via the platform shell so they run unchanged
//! in CI on Windows, macOS, and Linux.

use super::*;
use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use std::time::Duration;

/// A `ProcessConfig` that runs a child which exits immediately on every
/// platform (Unix: `sh -lc "exit 0"`; Windows: `cmd /C "exit 0"`).
fn quick_exit_config() -> ProcessConfig {
    ProcessConfig {
        command: CommandSpec::Shell("exit 0".to_string()),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    }
}

// ── Capability negotiation ──

#[test]
fn negotiate_reports_lifecycle_supported() {
    let caps = ObserverCapabilities::negotiate();
    assert!(caps.is_supported(EventCategory::Lifecycle));
    assert_eq!(
        caps.support(EventCategory::Lifecycle),
        CapabilitySupport::Supported
    );
    let entry = caps.category(EventCategory::Lifecycle);
    assert_eq!(entry.backend, "portable-lifecycle");
    assert!(!entry.reason.is_empty());
}

#[test]
fn negotiate_reports_syscall_categories_unavailable_with_reason() {
    let caps = ObserverCapabilities::negotiate();
    for category in [
        EventCategory::File,
        EventCategory::Network,
        EventCategory::Process,
    ] {
        let entry = caps.category(category);
        assert_eq!(
            entry.support,
            CapabilitySupport::Unavailable,
            "{} should be unavailable in Phase 1",
            category.as_str()
        );
        // Honest reason, mentioning the deferred Phase 3 backend.
        assert!(
            entry.reason.contains("Phase 3"),
            "{} reason must explain the deferral: {:?}",
            category.as_str(),
            entry.reason
        );
        assert!(!caps.is_supported(category));
    }
}

#[test]
fn syscall_categories_advertise_per_os_backend_name() {
    // #430: replace the catch-all `backend: "none"` / "(seccomp/eBPF/ETW)"
    // reason with per-OS detection helpers. The matrix still reports
    // Unavailable until Phase 3 backends actually land, but the backend
    // name now matches what's planned for the current target OS. This
    // makes Phase 4 downstream UX (the clud capability matrix) honest
    // about WHAT will land WHERE rather than implying coverage by
    // listing all three backends in the reason string.
    let caps = ObserverCapabilities::negotiate();
    let file = caps.category(EventCategory::File);
    let network = caps.category(EventCategory::Network);
    let process = caps.category(EventCategory::Process);

    #[cfg(target_os = "linux")]
    {
        assert_eq!(file.backend, "seccomp-user-notify");
        assert_eq!(network.backend, "ebpf");
        assert_eq!(process.backend, "seccomp-user-notify");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(file.backend, "etw");
        assert_eq!(network.backend, "etw");
        assert_eq!(process.backend, "etw");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(file.backend, "kqueue");
        assert_eq!(network.backend, "endpoint-security");
        assert_eq!(process.backend, "endpoint-security");
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        assert_eq!(file.backend, "none");
        assert_eq!(network.backend, "none");
        assert_eq!(process.backend, "none");
    }

    // The reason field no longer hardcodes the multi-backend "(seccomp/eBPF/ETW)"
    // literal — it points at the one backend planned for THIS OS.
    for entry in [file, network, process] {
        assert!(
            !entry.reason.contains("seccomp/eBPF/ETW"),
            "stale multi-backend reason: {:?}",
            entry.reason
        );
        assert!(
            entry.reason.contains("Phase 3"),
            "reason must keep the Phase 3 anchor: {:?}",
            entry.reason
        );
    }
}

#[test]
fn negotiate_covers_every_category_exactly_once() {
    let caps = ObserverCapabilities::negotiate();
    assert_eq!(caps.categories().len(), EventCategory::ALL.len());
    for category in EventCategory::ALL {
        // Does not panic — every category is present.
        let _ = caps.category(category);
    }
}

// ── Lifecycle baseline: started + exited ──

#[test]
fn observed_child_emits_started_then_exited() {
    let (process, subscriber) =
        NativeProcess::with_observer(quick_exit_config(), ObserverConfig::lifecycle());
    process.start().expect("spawn quick-exit child");
    let pid = process.pid().expect("child has a pid");
    let code = process
        .wait(Some(Duration::from_secs(30)))
        .expect("child exits");
    // Make sure the reaping path has flushed the exit event.
    process.close().ok();

    let events = subscriber.drain();
    assert_eq!(
        events.len(),
        2,
        "expected exactly started + exited, got {:?}",
        events
    );

    let started = &events[0];
    assert_eq!(started.category, EventCategory::Lifecycle);
    assert_eq!(started.kind, ObserverEventKind::Started);
    assert_eq!(started.kind.as_str(), "started");
    assert_eq!(started.pid, pid);

    let exited = &events[1];
    assert_eq!(exited.category, EventCategory::Lifecycle);
    assert_eq!(exited.kind, ObserverEventKind::Exited { exit_code: code });
    assert_eq!(exited.kind.as_str(), "exited");
    assert_eq!(exited.pid, pid);
    assert!(exited.timestamp_ms >= started.timestamp_ms);
}

#[test]
fn exited_event_is_emitted_exactly_once_across_paths() {
    // Drive every exit-observing path (wait + poll + close) and confirm the
    // guard collapses them to a single `exited`.
    let (process, subscriber) =
        NativeProcess::with_observer(quick_exit_config(), ObserverConfig::lifecycle());
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    let _ = process.poll();
    process.close().ok();

    let exited_count = subscriber
        .drain()
        .into_iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Exited { .. }))
        .count();
    assert_eq!(exited_count, 1, "exited must fire exactly once");
}

// ── Off by default ──

#[test]
fn no_events_when_observation_not_configured() {
    // `NativeProcess::new` attaches no observer. Run a child to completion
    // and prove the lifecycle hooks stayed inert (no channel, no events).
    let process = NativeProcess::new(quick_exit_config());
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    process.close().ok();
    // There is no subscriber to receive from — the proof is structural:
    // `new` returns only the process, never a subscriber handle. This test
    // exercises the off-by-default code path to ensure it does not panic.
}

#[test]
fn config_observes_only_requested_categories() {
    let lifecycle = ObserverConfig::lifecycle();
    assert!(lifecycle.observes(EventCategory::Lifecycle));
    assert!(!lifecycle.observes(EventCategory::File));

    let none = ObserverConfig::with_categories([]);
    assert!(!none.observes(EventCategory::Lifecycle));
}

#[test]
fn unobserved_category_produces_no_events() {
    // An observer that does NOT request lifecycle must stay silent even
    // though the process spawns and exits.
    let (process, subscriber) = NativeProcess::with_observer(
        quick_exit_config(),
        ObserverConfig::with_categories([EventCategory::File]),
    );
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    process.close().ok();
    assert!(
        subscriber.drain().is_empty(),
        "non-lifecycle observer must emit nothing in Phase 1"
    );
}

// ── Stable string forms ──

#[test]
fn category_and_support_string_forms_are_stable() {
    assert_eq!(EventCategory::Lifecycle.as_str(), "lifecycle");
    assert_eq!(EventCategory::File.as_str(), "file");
    assert_eq!(EventCategory::Network.as_str(), "network");
    assert_eq!(EventCategory::Process.as_str(), "process");
    assert_eq!(CapabilitySupport::Supported.as_str(), "supported");
    assert_eq!(CapabilitySupport::Partial.as_str(), "partial");
    assert_eq!(CapabilitySupport::Unavailable.as_str(), "unavailable");
}
