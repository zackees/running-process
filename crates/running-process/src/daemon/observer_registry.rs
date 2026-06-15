//! Per-session registry of observer sinks (#221 Phase 2 / #429).
//!
//! The registry mirrors [`crate::daemon::telemetry::TeeRegistry`] for
//! event-stream observer payloads. Each registered sink wraps a bounded
//! `std::sync::mpsc::sync_channel`. The session lifecycle code fans out an
//! [`crate::observer::ObserverEvent`] to every sink whose configured category
//! set includes the event's category.
//!
//! Registrations live on the per-session struct, so they survive the client's
//! IPC connection going away. Events that arrive while no consumer is
//! draining the channel are accounted for via:
//! - `DropOldest` backpressure: `try_send` on a full channel drops the new
//!   event and bumps `missed_events` (matches `EventTeeSink`).
//! - `Block` backpressure: blocking `send`. The emitter waits for room.
//!
//! Observer events are deliberately not replayed across reconnect; this is an
//! event-stream surface (the PTY/pipe backlog is the byte-stream analog).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SendError, SyncSender, TrySendError};
use std::sync::Mutex;

use crate::observer::{EventCategory, ObserverEvent};

/// Backpressure policy for a registered observer sink. Mirrors
/// [`crate::daemon::telemetry::TeeBackpressure`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverBackpressure {
    /// Never block the emitter. On a full channel the event is dropped and
    /// `missed_events` is incremented.
    DropOldest,
    /// Block the emitter until the channel has room.
    Block,
}

/// Subscription handle used to look up / remove a registered sink.
///
/// Internally a UUID v4 string assigned by the server. Stable across IPC
/// reconnect for the lifetime of the subscription.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ObserverSubscriberId(String);

impl ObserverSubscriberId {
    pub fn new() -> Self {
        Self(generate_uuid_v4_like())
    }

    pub fn from_string(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for ObserverSubscriberId {
    fn default() -> Self {
        Self::new()
    }
}

/// Current status of a registered observer sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObserverSinkStatus {
    /// Cumulative count of events that could not be delivered because the
    /// bounded channel was full (DropOldest) or never (Block).
    pub missed_events: u64,
    /// True once the downstream receiver has been dropped.
    pub disconnected: bool,
    /// Cumulative count of events that the registry successfully handed off
    /// to the bounded channel.
    pub delivered_events: u64,
}

/// One registered sink: a bounded channel plus its configured category
/// filter and backpressure policy.
struct ObserverSink {
    categories: Vec<EventCategory>,
    sender: SyncSender<ObserverEvent>,
    backpressure: ObserverBackpressure,
    missed_events: AtomicU64,
    delivered_events: AtomicU64,
    disconnected: AtomicUsize, // 0 = false, 1 = true
}

impl ObserverSink {
    fn matches(&self, category: EventCategory) -> bool {
        self.categories.contains(&category)
    }

    fn status(&self) -> ObserverSinkStatus {
        ObserverSinkStatus {
            missed_events: self.missed_events.load(Ordering::Relaxed),
            disconnected: self.disconnected.load(Ordering::Acquire) != 0,
            delivered_events: self.delivered_events.load(Ordering::Relaxed),
        }
    }

    fn push(&self, event: ObserverEvent) {
        if !self.matches(event.category) {
            return;
        }
        if self.disconnected.load(Ordering::Acquire) != 0 {
            self.missed_events.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match self.backpressure {
            ObserverBackpressure::DropOldest => match self.sender.try_send(event) {
                Ok(()) => {
                    self.delivered_events.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.missed_events.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.disconnected.store(1, Ordering::Release);
                    self.missed_events.fetch_add(1, Ordering::Relaxed);
                }
            },
            ObserverBackpressure::Block => match self.sender.send(event) {
                Ok(()) => {
                    self.delivered_events.fetch_add(1, Ordering::Relaxed);
                }
                Err(SendError(_)) => {
                    self.disconnected.store(1, Ordering::Release);
                    self.missed_events.fetch_add(1, Ordering::Relaxed);
                }
            },
        }
    }
}

/// Per-session registry of observer sinks.
///
/// Wrap one of these inside each daemon-owned session struct (PTY/pipe). The
/// session's lifecycle code calls [`ObserverRegistry::emit`] from the spawn
/// and reap paths; events fan out to every matching registered sink.
pub struct ObserverRegistry {
    active_sinks: AtomicUsize,
    sinks: Mutex<HashMap<ObserverSubscriberId, ObserverSink>>,
}

impl ObserverRegistry {
    pub fn new() -> Self {
        Self {
            active_sinks: AtomicUsize::new(0),
            sinks: Mutex::new(HashMap::new()),
        }
    }

    /// Register a bounded channel sink and return its server-assigned id +
    /// the consumer end of the channel.
    pub fn add_channel(
        &self,
        categories: Vec<EventCategory>,
        capacity: usize,
        backpressure: ObserverBackpressure,
    ) -> (ObserverSubscriberId, Receiver<ObserverEvent>) {
        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        let id = ObserverSubscriberId::new();
        let sink = ObserverSink {
            categories,
            sender: tx,
            backpressure,
            missed_events: AtomicU64::new(0),
            delivered_events: AtomicU64::new(0),
            disconnected: AtomicUsize::new(0),
        };
        self.sinks.lock().unwrap().insert(id.clone(), sink);
        self.active_sinks.fetch_add(1, Ordering::Release);
        (id, rx)
    }

    /// Remove a registered sink. Returns `true` if a sink was removed.
    pub fn remove(&self, id: &ObserverSubscriberId) -> bool {
        let removed = self.sinks.lock().unwrap().remove(id).is_some();
        if removed {
            self.active_sinks.fetch_sub(1, Ordering::Release);
        }
        removed
    }

    /// Fetch the current status for a registered sink.
    pub fn status(&self, id: &ObserverSubscriberId) -> Option<ObserverSinkStatus> {
        self.sinks.lock().unwrap().get(id).map(ObserverSink::status)
    }

    /// Emit one event to every registered sink whose category filter matches.
    ///
    /// Cold path when no sinks are attached: a single atomic load.
    pub fn emit(&self, event: ObserverEvent) {
        if self.active_sinks.load(Ordering::Acquire) == 0 {
            return;
        }
        let sinks = self.sinks.lock().unwrap();
        for sink in sinks.values() {
            sink.push(event.clone());
        }
    }

    /// Number of currently registered sinks.
    pub fn len(&self) -> usize {
        self.active_sinks.load(Ordering::Acquire)
    }

    /// True when no sinks are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ObserverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Produce a UUID v4 style string without pulling in a `uuid` dep.
///
/// 122 random bits + RFC 4122 v4 layout. The randomness source is the
/// system clock nanoseconds mixed with a per-call counter — enough for
/// in-process uniqueness, which is the contract for subscriber ids.
fn generate_uuid_v4_like() -> String {
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // SplitMix64 of (nanos ^ counter) gives us a reasonable 64-bit mix.
    let lo = splitmix64(nanos ^ counter);
    let hi = splitmix64(lo.wrapping_add(counter.wrapping_mul(0x9E3779B97F4A7C15)));

    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_le_bytes());
    bytes[8..].copy_from_slice(&lo.to_le_bytes());
    // Layout: version 4 (random) + RFC 4122 variant bits.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11],
        bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};
    use std::time::Duration;

    fn lifecycle_started(pid: u32) -> ObserverEvent {
        // Construct via the public constructor by mirroring the same shape;
        // ObserverEvent fields are pub.
        ObserverEvent {
            category: EventCategory::Lifecycle,
            kind: ObserverEventKind::Started,
            pid,
            timestamp_ms: 0,
        }
    }

    fn file_event(pid: u32) -> ObserverEvent {
        ObserverEvent {
            category: EventCategory::File,
            kind: ObserverEventKind::Started,
            pid,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn registry_emits_to_subscribed_sinks_only() {
        let reg = ObserverRegistry::new();
        let (_lifecycle_id, lifecycle_rx) = reg.add_channel(
            vec![EventCategory::Lifecycle],
            4,
            ObserverBackpressure::Block,
        );
        let (_file_id, file_rx) =
            reg.add_channel(vec![EventCategory::File], 4, ObserverBackpressure::Block);

        reg.emit(lifecycle_started(101));

        let lifecycle_event = lifecycle_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lifecycle sink should receive the event");
        assert_eq!(lifecycle_event.category, EventCategory::Lifecycle);
        assert_eq!(lifecycle_event.pid, 101);

        // The file sink filter does not include Lifecycle: it must not see
        // this event.
        assert!(
            file_rx.try_recv().is_err(),
            "file sink should not receive a lifecycle event"
        );

        // Sanity: the file sink does receive a file event.
        reg.emit(file_event(202));
        let file_event_received = file_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("file sink should receive a file event");
        assert_eq!(file_event_received.category, EventCategory::File);
        assert_eq!(file_event_received.pid, 202);
    }

    #[test]
    fn dropoldest_increments_missed_when_consumer_slow() {
        let reg = ObserverRegistry::new();
        let (id, _rx) = reg.add_channel(
            vec![EventCategory::Lifecycle],
            2,
            ObserverBackpressure::DropOldest,
        );

        // Capacity 2 + never drain → first two land, next three are missed.
        for pid in 0..5 {
            reg.emit(lifecycle_started(pid));
        }

        let status = reg.status(&id).expect("sink should still be registered");
        assert_eq!(status.missed_events, 3, "expected 3 missed events");
        assert_eq!(status.delivered_events, 2, "expected 2 delivered events");
        assert!(!status.disconnected);
    }

    #[test]
    fn unregister_removes_sink() {
        let reg = ObserverRegistry::new();
        let (id, rx) = reg.add_channel(
            vec![EventCategory::Lifecycle],
            4,
            ObserverBackpressure::Block,
        );
        assert!(reg.remove(&id));
        // Subsequent emit must not panic, must not deliver to the removed
        // sink, and must keep the registry empty.
        reg.emit(lifecycle_started(7));
        assert!(rx.try_recv().is_err());
        assert!(reg.is_empty());
        // Double-remove is a no-op.
        assert!(!reg.remove(&id));
    }

    #[test]
    fn empty_registry_emit_is_cheap_and_safe() {
        let reg = ObserverRegistry::new();
        // No sinks, no panic, no allocation surprise. Hot path is a single
        // atomic load — this just checks the no-op path is safe.
        reg.emit(lifecycle_started(1));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn generated_subscriber_ids_are_unique() {
        let a = ObserverSubscriberId::new();
        let b = ObserverSubscriberId::new();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 36); // 36 = canonical UUID with hyphens.
    }
}
