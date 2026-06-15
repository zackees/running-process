//! Phase 1 of #221: the process-observation capability model and the
//! portable process-lifecycle baseline.
//!
//! This module defines the stable observation types — [`ObserverConfig`],
//! [`ObserverCapabilities`], [`ObserverEvent`], and the
//! [`ObserverSubscriber`] handle — plus the always-available lifecycle
//! backend that emits [`started`](ObserverEventKind::Started) and
//! [`exited`](ObserverEventKind::Exited) events for child processes spawned
//! by this crate.
//!
//! ## Scope (Phase 1 only)
//!
//! Only the [`EventCategory::Lifecycle`] category is
//! [`supported`](CapabilitySupport::Supported). Every other category
//! ([`File`](EventCategory::File), [`Network`](EventCategory::Network),
//! [`Process`](EventCategory::Process)) reports
//! [`unavailable`](CapabilitySupport::Unavailable) with an honest reason,
//! because syscall-level backends (seccomp/eBPF/ETW) are Phase 3 work and
//! are deliberately not wired here.
//!
//! ## Off by default
//!
//! Observation is entirely opt-in. A [`NativeProcess`](crate::NativeProcess)
//! emits no events unless an [`ObserverConfig`] is attached via
//! [`NativeProcess::with_observer`](crate::NativeProcess::with_observer) (or
//! the equivalent builder seam). With no observer configured the lifecycle
//! hooks are inert: no channel, no allocation, no events.
//!
//! The handle is a plain `std::sync::mpsc` receiver so the lifecycle
//! baseline stays free of the daemon runtime (tokio/IPC). Phase 2 layers the
//! daemon-owned subscriber model on top of these same event types.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

/// Category of observable process activity.
///
/// Phase 1 only implements [`Lifecycle`](Self::Lifecycle). The remaining
/// categories exist so capability negotiation can report them as
/// `unavailable` with an honest reason until their Phase 3 platform backends
/// land.
///
/// Marked `#[non_exhaustive]` per #431: Phase 3 will refine these categories
/// (and possibly add sub-categories) without forcing every consumer to bump
/// to a new major version of the crate. Out-of-crate matchers must include a
/// wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCategory {
    /// Process start and exit for children spawned by this crate.
    Lifecycle,
    /// Filesystem activity (open/read/write/unlink). Requires a Phase 3
    /// platform backend.
    File,
    /// Network activity (connect/accept/send/recv). Requires a Phase 3
    /// platform backend.
    Network,
    /// Descendant process creation outside the crate's own spawn path.
    /// Requires a Phase 3 platform backend.
    Process,
}

impl EventCategory {
    /// All categories the capability matrix reports on, in a stable order.
    pub const ALL: [EventCategory; 4] = [
        EventCategory::Lifecycle,
        EventCategory::File,
        EventCategory::Network,
        EventCategory::Process,
    ];

    /// Return the stable lowercase category name.
    pub fn as_str(self) -> &'static str {
        match self {
            EventCategory::Lifecycle => "lifecycle",
            EventCategory::File => "file",
            EventCategory::Network => "network",
            EventCategory::Process => "process",
        }
    }
}

/// Negotiated support level for a single [`EventCategory`].
///
/// Marked `#[non_exhaustive]` per #431: later phases may introduce richer
/// support gradations (e.g. a `Degraded` variant distinct from `Partial`)
/// without breaking out-of-crate matchers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySupport {
    /// The category is fully observable on this platform.
    Supported,
    /// The category is observable but with documented gaps or caveats.
    Partial,
    /// The category cannot be observed by the active backend set.
    Unavailable,
}

impl CapabilitySupport {
    /// Return the stable lowercase support-level name.
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilitySupport::Supported => "supported",
            CapabilitySupport::Partial => "partial",
            CapabilitySupport::Unavailable => "unavailable",
        }
    }
}

/// Capability report for one [`EventCategory`]: the negotiated support
/// level, the backend that would serve it, and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryCapability {
    /// Which category this entry describes.
    pub category: EventCategory,
    /// Negotiated support level.
    pub support: CapabilitySupport,
    /// Name of the backend serving (or that would serve) this category.
    pub backend: &'static str,
    /// Human-readable explanation, especially for `Partial`/`Unavailable`.
    pub reason: &'static str,
}

/// The full capability matrix produced by [`ObserverCapabilities::negotiate`].
///
/// Each [`EventCategory`] appears exactly once. Phase 1 reports
/// [`Lifecycle`](EventCategory::Lifecycle) as
/// [`Supported`](CapabilitySupport::Supported) and the rest as
/// [`Unavailable`](CapabilitySupport::Unavailable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverCapabilities {
    categories: Vec<CategoryCapability>,
}

/// Detect the backend that would serve [`EventCategory::File`] on this
/// platform (#430 prep for Phase 3).
///
/// Returns `(support, backend, reason)`. Today every branch returns
/// `Unavailable` because no Phase 3 backend has shipped yet — but the
/// backend name and reason are now per-OS, so downstream UX (Phase 4)
/// shows the right deferred-backend name instead of the catch-all
/// `seccomp/eBPF/ETW` literal. As individual backends land, flip the
/// matching branch to `Supported`/`Partial` with no shape change.
fn detect_file_backend() -> (CapabilitySupport, &'static str, &'static str) {
    #[cfg(target_os = "linux")]
    {
        (
            CapabilitySupport::Unavailable,
            "seccomp-user-notify",
            "Phase 3: Linux seccomp user-notify file backend not yet implemented",
        )
    }
    #[cfg(target_os = "windows")]
    {
        (
            CapabilitySupport::Unavailable,
            "etw",
            "Phase 3: Windows ETW file backend not yet implemented",
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            CapabilitySupport::Unavailable,
            "kqueue",
            "Phase 3: macOS kqueue/EndpointSecurity file backend not yet implemented (entitlement-gated)",
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        (
            CapabilitySupport::Unavailable,
            "none",
            "Phase 3: no file backend planned for this OS",
        )
    }
}

/// Detect the backend that would serve [`EventCategory::Network`] on this
/// platform (#430 prep for Phase 3). Mirrors [`detect_file_backend`].
fn detect_network_backend() -> (CapabilitySupport, &'static str, &'static str) {
    #[cfg(target_os = "linux")]
    {
        (
            CapabilitySupport::Unavailable,
            "ebpf",
            "Phase 3: Linux eBPF network backend not yet implemented",
        )
    }
    #[cfg(target_os = "windows")]
    {
        (
            CapabilitySupport::Unavailable,
            "etw",
            "Phase 3: Windows ETW network backend not yet implemented",
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            CapabilitySupport::Unavailable,
            "endpoint-security",
            "Phase 3: macOS EndpointSecurity network backend not yet implemented (entitlement-gated)",
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        (
            CapabilitySupport::Unavailable,
            "none",
            "Phase 3: no network backend planned for this OS",
        )
    }
}

/// Detect the backend that would serve [`EventCategory::Process`] (descendant
/// process creation outside the crate's own spawn path) on this platform
/// (#430 prep for Phase 3). Mirrors [`detect_file_backend`].
fn detect_process_backend() -> (CapabilitySupport, &'static str, &'static str) {
    #[cfg(target_os = "linux")]
    {
        (
            CapabilitySupport::Unavailable,
            "seccomp-user-notify",
            "Phase 3: Linux seccomp user-notify process backend not yet implemented",
        )
    }
    #[cfg(target_os = "windows")]
    {
        (
            CapabilitySupport::Unavailable,
            "etw",
            "Phase 3: Windows ETW process backend not yet implemented",
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            CapabilitySupport::Unavailable,
            "endpoint-security",
            "Phase 3: macOS EndpointSecurity process backend not yet implemented (entitlement-gated)",
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        (
            CapabilitySupport::Unavailable,
            "none",
            "Phase 3: no process backend planned for this OS",
        )
    }
}

impl ObserverCapabilities {
    /// Negotiate the capability matrix for the current platform.
    ///
    /// Phase 1 reports `Lifecycle` as `Supported` (portable, OS-agnostic).
    /// Phase 3 categories (`File`, `Network`, `Process`) currently report
    /// `Unavailable`, but the *backend name* and *reason* are now per-OS via
    /// `#[cfg]`-gated detection helpers (#430). This keeps the
    /// `ObserverCapabilities::negotiate()` contract stable for Phase 4
    /// downstream UX while letting Phase 3 light each backend up
    /// independently — flipping `Unavailable` → `Supported` per backend lands
    /// without touching this function's shape.
    pub fn negotiate() -> Self {
        let categories = EventCategory::ALL
            .iter()
            .map(|&category| match category {
                EventCategory::Lifecycle => CategoryCapability {
                    category,
                    support: CapabilitySupport::Supported,
                    backend: "portable-lifecycle",
                    reason: "started/exited emitted from the crate spawn and reap path",
                },
                EventCategory::File => {
                    let (support, backend, reason) = detect_file_backend();
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
                EventCategory::Network => {
                    let (support, backend, reason) = detect_network_backend();
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
                EventCategory::Process => {
                    let (support, backend, reason) = detect_process_backend();
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
            })
            .collect();
        Self { categories }
    }

    /// Return the capability entries in stable [`EventCategory::ALL`] order.
    pub fn categories(&self) -> &[CategoryCapability] {
        &self.categories
    }

    /// Look up the capability entry for one category.
    pub fn category(&self, category: EventCategory) -> &CategoryCapability {
        self.categories
            .iter()
            .find(|entry| entry.category == category)
            .expect("ObserverCapabilities always contains every EventCategory")
    }

    /// Return the negotiated support level for one category.
    pub fn support(&self, category: EventCategory) -> CapabilitySupport {
        self.category(category).support
    }

    /// Return whether a category is fully [`Supported`](CapabilitySupport::Supported).
    pub fn is_supported(&self, category: EventCategory) -> bool {
        self.support(category) == CapabilitySupport::Supported
    }

    /// Return the capability matrix as four fixed-width rows suitable for
    /// downstream UX (e.g. a clud CLI flag — see Phase 4 of #221 / #431).
    ///
    /// Each row is `[category, support, backend, reason]`. Row order matches
    /// [`EventCategory::ALL`], so consumers can rely on a stable layout. The
    /// strings are owned so callers can paint colors / pad columns without
    /// borrowing from `self`.
    pub fn to_table_rows(&self) -> Vec<[String; 4]> {
        self.categories
            .iter()
            .map(|entry| {
                [
                    entry.category.as_str().to_string(),
                    entry.support.as_str().to_string(),
                    entry.backend.to_string(),
                    entry.reason.to_string(),
                ]
            })
            .collect()
    }

    /// Render the capability matrix as a single human-readable string.
    ///
    /// The output is deterministic per category set so a UI can snapshot or
    /// diff it. Layout:
    ///
    /// ```text
    /// observer capabilities:
    ///   lifecycle    supported    portable-lifecycle  started/exited emitted from the crate spawn and reap path
    ///   file         unavailable  none                requires Phase 3 platform backend (seccomp/eBPF/ETW)
    ///   network      unavailable  none                requires Phase 3 platform backend (seccomp/eBPF/ETW)
    ///   process      unavailable  none                requires Phase 3 platform backend (seccomp/eBPF/ETW)
    /// ```
    ///
    /// Phase 4 (#431) consumers like the clud CLI use this to show the
    /// actually negotiated matrix rather than claiming syscall coverage the
    /// active backends do not provide.
    pub fn render_summary(&self) -> String {
        // Compute column widths from the longest entry per column so the
        // output stays aligned as future categories / backends land.
        let rows = self.to_table_rows();
        let mut widths = [0usize; 3];
        for row in &rows {
            for (i, cell) in row[..3].iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let mut out = String::from("observer capabilities:\n");
        for row in &rows {
            out.push_str(&format!(
                "  {cat:<cw$}  {sup:<sw$}  {bk:<bw$}  {reason}\n",
                cat = row[0],
                sup = row[1],
                bk = row[2],
                reason = row[3],
                cw = widths[0],
                sw = widths[1],
                bw = widths[2],
            ));
        }
        out
    }
}

/// What happened to an observed process.
///
/// Marked `#[non_exhaustive]` per #431: Phase 3 will add variants for File,
/// Network, and Process events. Out-of-crate matchers must include a
/// wildcard arm to remain forward-compatible across minor releases.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverEventKind {
    /// The child process was spawned. Carries no extra payload.
    Started,
    /// The child process exited. Carries the OS exit code (Unix signal
    /// exits are negative signal numbers, matching the rest of the crate).
    Exited {
        /// Exit code of the child.
        exit_code: i32,
    },
}

impl ObserverEventKind {
    /// Return the stable lowercase event-kind name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObserverEventKind::Started => "started",
            ObserverEventKind::Exited { .. } => "exited",
        }
    }
}

/// A single observation emitted by the lifecycle baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverEvent {
    /// Which category produced the event. Always
    /// [`EventCategory::Lifecycle`] in Phase 1.
    pub category: EventCategory,
    /// What happened.
    pub kind: ObserverEventKind,
    /// OS process id of the observed child.
    pub pid: u32,
    /// Milliseconds since the Unix epoch when the event was recorded.
    pub timestamp_ms: u128,
}

impl ObserverEvent {
    /// Construct an event, stamping it with the current wall-clock time.
    fn now(category: EventCategory, kind: ObserverEventKind, pid: u32) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            category,
            kind,
            pid,
            timestamp_ms,
        }
    }
}

/// Opt-in configuration that turns process observation on for a single
/// [`NativeProcess`](crate::NativeProcess).
///
/// Constructing a config does not by itself observe anything; it is attached
/// to a process via
/// [`NativeProcess::with_observer`](crate::NativeProcess::with_observer).
/// With no config attached, the process emits no events (off by default).
#[derive(Debug, Clone)]
pub struct ObserverConfig {
    categories: Vec<EventCategory>,
}

impl ObserverConfig {
    /// Create a config that observes only the Phase 1 lifecycle baseline.
    ///
    /// This is the recommended Phase 1 constructor: it requests exactly the
    /// category that is actually `Supported`.
    pub fn lifecycle() -> Self {
        Self {
            categories: vec![EventCategory::Lifecycle],
        }
    }

    /// Create a config requesting an explicit set of categories.
    ///
    /// Categories that are not `Supported` on this platform simply never
    /// produce events in Phase 1; callers should consult
    /// [`ObserverCapabilities::negotiate`] to learn which ones are honored.
    pub fn with_categories(categories: impl IntoIterator<Item = EventCategory>) -> Self {
        Self {
            categories: categories.into_iter().collect(),
        }
    }

    /// Return whether this config requested observation of `category`.
    pub fn observes(&self, category: EventCategory) -> bool {
        self.categories.contains(&category)
    }

    /// The categories this config requested, in insertion order.
    pub fn categories(&self) -> &[EventCategory] {
        &self.categories
    }
}

/// Receiver handle for observation events.
///
/// Returned by
/// [`NativeProcess::with_observer`](crate::NativeProcess::with_observer).
/// Dropping the subscriber detaches it; the emitter tolerates a closed
/// channel and never blocks on a slow or absent consumer.
pub struct ObserverSubscriber {
    rx: Receiver<ObserverEvent>,
}

impl ObserverSubscriber {
    /// Receive the next event, blocking until one arrives or the emitter is
    /// dropped. Returns `None` once no more events can arrive.
    pub fn recv(&self) -> Option<ObserverEvent> {
        self.rx.recv().ok()
    }

    /// Try to receive an event without blocking.
    pub fn try_recv(&self) -> Option<ObserverEvent> {
        self.rx.try_recv().ok()
    }

    /// Drain all currently-queued events without blocking.
    pub fn drain(&self) -> Vec<ObserverEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Borrow the underlying receiver for advanced use (e.g. `iter`/`select`).
    pub fn receiver(&self) -> &Receiver<ObserverEvent> {
        &self.rx
    }
}

/// Internal emitter held by a [`NativeProcess`](crate::NativeProcess) when an
/// [`ObserverConfig`] is attached.
///
/// `None` on a process means observation is off, so the lifecycle hooks are
/// inert. This keeps the off-by-default path allocation-free.
pub(crate) struct ObserverEmitter {
    config: ObserverConfig,
    tx: Sender<ObserverEvent>,
}

impl ObserverEmitter {
    /// Build an emitter from a config and hand back the paired subscriber.
    pub(crate) fn new(config: ObserverConfig) -> (Self, ObserverSubscriber) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { config, tx }, ObserverSubscriber { rx })
    }

    /// Emit a `started` event for `pid` if the config observes lifecycle.
    pub(crate) fn emit_started(&self, pid: u32) {
        if !self.config.observes(EventCategory::Lifecycle) {
            return;
        }
        // Ignore send errors: a dropped subscriber must never break the
        // process spawn/reap path.
        let _ = self.tx.send(ObserverEvent::now(
            EventCategory::Lifecycle,
            ObserverEventKind::Started,
            pid,
        ));
    }

    /// Emit an `exited` event for `pid` if the config observes lifecycle.
    pub(crate) fn emit_exited(&self, pid: u32, exit_code: i32) {
        if !self.config.observes(EventCategory::Lifecycle) {
            return;
        }
        let _ = self.tx.send(ObserverEvent::now(
            EventCategory::Lifecycle,
            ObserverEventKind::Exited { exit_code },
            pid,
        ));
    }
}

#[cfg(test)]
mod tests;
