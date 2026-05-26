//! Opt-in session telemetry primitives for daemon-owned sessions.
//!
//! This is the first #131 slice: bounded in-memory tee rings with explicit
//! drop-oldest backpressure. File, raw-handle, and callback sinks can build
//! on the same registry shape without changing the reader hot path.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Opaque identifier for a registered tee sink.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TeeHandle(u64);

impl TeeHandle {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Stream observed by a tee sink.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TeeStream {
    /// Combined PTY output bytes as emitted by the platform PTY backend.
    PtyOutput,
    Stdout,
    Stderr,
    /// Echo of bytes successfully written to the child's stdin.
    Stdin,
}

/// Backpressure behavior for bounded tee sinks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeeBackpressure {
    /// Never block the child/session reader; drop oldest retained bytes.
    DropOldest,
}

/// Options used when registering a tee sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeeOptions {
    pub backpressure: TeeBackpressure,
}

impl Default for TeeOptions {
    fn default() -> Self {
        Self {
            backpressure: TeeBackpressure::DropOldest,
        }
    }
}

/// Snapshot of a bounded tee ring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeeSnapshot {
    pub stream: TeeStream,
    pub bytes: Vec<u8>,
    pub missed_bytes: u64,
    pub capacity: usize,
}

/// Per-session registry of tee sinks.
pub struct TeeRegistry {
    next_id: AtomicU64,
    active_sinks: AtomicUsize,
    sinks: Mutex<HashMap<TeeHandle, TeeSink>>,
}

impl TeeRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            active_sinks: AtomicUsize::new(0),
            sinks: Mutex::new(HashMap::new()),
        }
    }

    /// Register a bounded in-memory ring sink using the default drop policy.
    pub fn add_ring(&self, stream: TeeStream, capacity: usize) -> TeeHandle {
        self.add_ring_with_options(stream, capacity, TeeOptions::default())
    }

    /// Register a bounded in-memory ring sink.
    pub fn add_ring_with_options(
        &self,
        stream: TeeStream,
        capacity: usize,
        options: TeeOptions,
    ) -> TeeHandle {
        match options.backpressure {
            TeeBackpressure::DropOldest => {}
        }

        let handle = TeeHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        let sink = TeeSink {
            stream,
            ring: RingTeeSink::new(capacity),
        };
        self.sinks.lock().unwrap().insert(handle, sink);
        self.active_sinks.fetch_add(1, Ordering::Release);
        handle
    }

    /// Remove a sink by handle. Returns true when a sink was removed.
    pub fn remove(&self, handle: TeeHandle) -> bool {
        let removed = self.sinks.lock().unwrap().remove(&handle).is_some();
        if removed {
            self.active_sinks.fetch_sub(1, Ordering::Release);
        }
        removed
    }

    /// Snapshot a ring sink without draining it.
    pub fn snapshot(&self, handle: TeeHandle) -> Option<TeeSnapshot> {
        self.sinks
            .lock()
            .unwrap()
            .get(&handle)
            .map(TeeSink::snapshot)
    }

    /// Tee bytes to all sinks registered for `stream`.
    pub fn write(&self, stream: TeeStream, bytes: &[u8]) {
        if bytes.is_empty() || self.active_sinks.load(Ordering::Acquire) == 0 {
            return;
        }

        let mut sinks = self.sinks.lock().unwrap();
        for sink in sinks.values_mut().filter(|sink| sink.stream == stream) {
            sink.ring.push(bytes);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active_sinks.load(Ordering::Acquire) == 0
    }
}

impl Default for TeeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct TeeSink {
    stream: TeeStream,
    ring: RingTeeSink,
}

impl TeeSink {
    fn snapshot(&self) -> TeeSnapshot {
        let (bytes, missed_bytes) = self.ring.snapshot();
        TeeSnapshot {
            stream: self.stream,
            bytes,
            missed_bytes,
            capacity: self.ring.capacity,
        }
    }
}

struct RingTeeSink {
    capacity: usize,
    data: VecDeque<u8>,
    missed_bytes: u64,
}

impl RingTeeSink {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            missed_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.capacity == 0 {
            self.missed_bytes += bytes.len() as u64;
            return;
        }

        let (slice, extra_missed) = if bytes.len() > self.capacity {
            let drop_n = bytes.len() - self.capacity;
            (&bytes[drop_n..], drop_n as u64)
        } else {
            (bytes, 0)
        };

        let overflow = (self.data.len() + slice.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.data.drain(..overflow);
            self.missed_bytes += overflow as u64;
        }
        self.missed_bytes += extra_missed;
        self.data.extend(slice);
    }

    fn snapshot(&self) -> (Vec<u8>, u64) {
        (self.data.iter().copied().collect(), self.missed_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_tee_keeps_tail_and_reports_missed_bytes() {
        let registry = TeeRegistry::new();
        let handle = registry.add_ring(TeeStream::Stdout, 5);

        registry.write(TeeStream::Stdout, b"abc");
        registry.write(TeeStream::Stdout, b"defgh");

        let snapshot = registry.snapshot(handle).expect("snapshot");
        assert_eq!(snapshot.stream, TeeStream::Stdout);
        assert_eq!(snapshot.bytes, b"defgh");
        assert_eq!(snapshot.missed_bytes, 3);
        assert_eq!(snapshot.capacity, 5);
    }

    #[test]
    fn rings_are_stream_specific() {
        let registry = TeeRegistry::new();
        let stdout = registry.add_ring(TeeStream::Stdout, 64);
        let stderr = registry.add_ring(TeeStream::Stderr, 64);

        registry.write(TeeStream::Stdout, b"out");
        registry.write(TeeStream::Stderr, b"err");

        assert_eq!(registry.snapshot(stdout).unwrap().bytes, b"out");
        assert_eq!(registry.snapshot(stderr).unwrap().bytes, b"err");
    }

    #[test]
    fn multiple_rings_receive_identical_bytes() {
        let registry = TeeRegistry::new();
        let a = registry.add_ring(TeeStream::PtyOutput, 64);
        let b = registry.add_ring(TeeStream::PtyOutput, 64);

        registry.write(TeeStream::PtyOutput, b"pty bytes");

        assert_eq!(registry.snapshot(a).unwrap().bytes, b"pty bytes");
        assert_eq!(registry.snapshot(b).unwrap().bytes, b"pty bytes");
    }

    #[test]
    fn removed_ring_stops_receiving_bytes() {
        let registry = TeeRegistry::new();
        let handle = registry.add_ring(TeeStream::Stdout, 64);

        registry.write(TeeStream::Stdout, b"before");
        assert!(registry.remove(handle));
        registry.write(TeeStream::Stdout, b"after");

        assert!(registry.snapshot(handle).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn zero_capacity_ring_reports_every_byte_missed() {
        let registry = TeeRegistry::new();
        let handle = registry.add_ring(TeeStream::Stdout, 0);

        registry.write(TeeStream::Stdout, b"abc");

        let snapshot = registry.snapshot(handle).unwrap();
        assert!(snapshot.bytes.is_empty());
        assert_eq!(snapshot.missed_bytes, 3);
    }
}
