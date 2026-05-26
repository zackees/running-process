//! Opt-in session telemetry primitives for daemon-owned sessions.
//!
//! This is the first #131 slice: bounded in-memory tee rings with explicit
//! drop-oldest backpressure. File, raw-handle, and callback sinks can build
//! on the same registry shape without changing the reader hot path.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;
use std::thread;

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

/// Event delivered by non-blocking queued tee sinks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeeEvent {
    Bytes(Vec<u8>),
    MissedBytes(u64),
}

/// Current sink status. Useful for queued sinks whose byte events are
/// consumed out-of-band.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeeStatus {
    pub stream: TeeStream,
    pub missed_bytes: u64,
    pub disconnected: bool,
}

/// File sink open mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeeFileMode {
    Append,
    Truncate,
}

/// Options for file path tee sinks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeeFileOptions {
    pub mode: TeeFileMode,
    pub queue_capacity: usize,
    pub write_missed_markers: bool,
}

impl Default for TeeFileOptions {
    fn default() -> Self {
        Self {
            mode: TeeFileMode::Append,
            queue_capacity: 1024,
            write_missed_markers: true,
        }
    }
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

        let sink = TeeSink {
            stream,
            kind: TeeSinkKind::Ring(RingTeeSink::new(capacity)),
        };
        self.insert_sink(sink)
    }

    /// Register a bounded non-blocking channel sink.
    pub fn add_channel(
        &self,
        stream: TeeStream,
        capacity: usize,
    ) -> (TeeHandle, Receiver<TeeEvent>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let sink = TeeSink {
            stream,
            kind: TeeSinkKind::Event(EventTeeSink::new(sender)),
        };
        (self.insert_sink(sink), receiver)
    }

    /// Register a callback sink backed by a bounded non-blocking channel.
    pub fn add_callback<F>(&self, stream: TeeStream, capacity: usize, mut callback: F) -> TeeHandle
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        let (handle, receiver) = self.add_channel(stream, capacity);
        thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                callback(event);
            }
        });
        handle
    }

    /// Register a file path sink backed by a bounded non-blocking channel.
    pub fn add_file<P>(
        &self,
        stream: TeeStream,
        path: P,
        options: TeeFileOptions,
    ) -> io::Result<TeeHandle>
    where
        P: AsRef<Path>,
    {
        let mut open = OpenOptions::new();
        open.create(true).write(true);
        match options.mode {
            TeeFileMode::Append => {
                open.append(true);
            }
            TeeFileMode::Truncate => {
                open.truncate(true);
            }
        }
        let mut file = open.open(path)?;
        let (handle, receiver) = self.add_channel(stream, options.queue_capacity);
        thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                let write_result = match event {
                    TeeEvent::Bytes(bytes) => file.write_all(&bytes),
                    TeeEvent::MissedBytes(n) if options.write_missed_markers => {
                        let marker = format!("\n[running-process tee missed {n} bytes]\n");
                        file.write_all(marker.as_bytes())
                    }
                    TeeEvent::MissedBytes(_) => Ok(()),
                };
                if write_result.is_err() || file.flush().is_err() {
                    break;
                }
            }
            let _ = file.flush();
        });
        Ok(handle)
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
            .and_then(TeeSink::snapshot)
    }

    /// Return the current missed-byte status for any sink type.
    pub fn status(&self, handle: TeeHandle) -> Option<TeeStatus> {
        self.sinks.lock().unwrap().get(&handle).map(TeeSink::status)
    }

    /// Tee bytes to all sinks registered for `stream`.
    pub fn write(&self, stream: TeeStream, bytes: &[u8]) {
        if bytes.is_empty() || self.active_sinks.load(Ordering::Acquire) == 0 {
            return;
        }

        let mut sinks = self.sinks.lock().unwrap();
        for sink in sinks.values_mut().filter(|sink| sink.stream == stream) {
            sink.push(bytes);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active_sinks.load(Ordering::Acquire) == 0
    }

    fn insert_sink(&self, sink: TeeSink) -> TeeHandle {
        let handle = TeeHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.sinks.lock().unwrap().insert(handle, sink);
        self.active_sinks.fetch_add(1, Ordering::Release);
        handle
    }
}

impl Default for TeeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct TeeSink {
    stream: TeeStream,
    kind: TeeSinkKind,
}

enum TeeSinkKind {
    Ring(RingTeeSink),
    Event(EventTeeSink),
}

impl TeeSink {
    fn snapshot(&self) -> Option<TeeSnapshot> {
        match &self.kind {
            TeeSinkKind::Ring(ring) => {
                let (bytes, missed_bytes) = ring.snapshot();
                Some(TeeSnapshot {
                    stream: self.stream,
                    bytes,
                    missed_bytes,
                    capacity: ring.capacity,
                })
            }
            TeeSinkKind::Event(_) => None,
        }
    }

    fn status(&self) -> TeeStatus {
        match &self.kind {
            TeeSinkKind::Ring(ring) => TeeStatus {
                stream: self.stream,
                missed_bytes: ring.missed_bytes,
                disconnected: false,
            },
            TeeSinkKind::Event(event) => TeeStatus {
                stream: self.stream,
                missed_bytes: event.missed_bytes,
                disconnected: event.disconnected,
            },
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        match &mut self.kind {
            TeeSinkKind::Ring(ring) => ring.push(bytes),
            TeeSinkKind::Event(event) => event.push(bytes),
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

struct EventTeeSink {
    sender: SyncSender<TeeEvent>,
    missed_bytes: u64,
    pending_missed: u64,
    disconnected: bool,
}

impl EventTeeSink {
    fn new(sender: SyncSender<TeeEvent>) -> Self {
        Self {
            sender,
            missed_bytes: 0,
            pending_missed: 0,
            disconnected: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.disconnected {
            self.record_missed(bytes.len() as u64);
            return;
        }

        if self.pending_missed > 0 {
            let missed = self.pending_missed;
            match self.sender.try_send(TeeEvent::MissedBytes(missed)) {
                Ok(()) => self.pending_missed = 0,
                Err(TrySendError::Full(_)) => {
                    self.record_missed(bytes.len() as u64);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.disconnected = true;
                    self.record_missed(bytes.len() as u64);
                    return;
                }
            }
        }

        match self.sender.try_send(TeeEvent::Bytes(bytes.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(TeeEvent::Bytes(bytes))) => {
                self.record_missed(bytes.len() as u64);
            }
            Err(TrySendError::Full(TeeEvent::MissedBytes(n))) => {
                self.record_missed(n);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.disconnected = true;
                self.record_missed(bytes.len() as u64);
            }
        }
    }

    fn record_missed(&mut self, n: u64) {
        self.missed_bytes += n;
        self.pending_missed += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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

    #[test]
    fn channel_sink_reports_missed_bytes_out_of_band() {
        let registry = TeeRegistry::new();
        let (handle, receiver) = registry.add_channel(TeeStream::Stdout, 2);

        registry.write(TeeStream::Stdout, b"a");
        registry.write(TeeStream::Stdout, b"b");
        registry.write(TeeStream::Stdout, b"c");

        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            TeeEvent::Bytes(b"a".to_vec())
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            TeeEvent::Bytes(b"b".to_vec())
        );

        registry.write(TeeStream::Stdout, b"d");

        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            TeeEvent::MissedBytes(1)
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            TeeEvent::Bytes(b"d".to_vec())
        );

        let status = registry.status(handle).expect("status");
        assert_eq!(status.stream, TeeStream::Stdout);
        assert_eq!(status.missed_bytes, 1);
        assert!(!status.disconnected);
        assert!(registry.snapshot(handle).is_none());
    }

    #[test]
    fn channel_sink_marks_disconnected_receivers() {
        let registry = TeeRegistry::new();
        let (handle, receiver) = registry.add_channel(TeeStream::Stdout, 1);
        drop(receiver);

        registry.write(TeeStream::Stdout, b"abc");

        let status = registry.status(handle).expect("status");
        assert_eq!(status.missed_bytes, 3);
        assert!(status.disconnected);
    }

    #[test]
    fn callback_sink_receives_events_on_worker_thread() {
        let registry = TeeRegistry::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_callback = Arc::clone(&seen);
        let handle = registry.add_callback(TeeStream::Stdout, 4, move |event| {
            seen_for_callback.lock().unwrap().push(event);
        });

        registry.write(TeeStream::Stdout, b"callback bytes");

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if seen.lock().unwrap().len() == 1 {
                break;
            }
            if Instant::now() >= deadline {
                panic!("callback did not receive tee event");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[TeeEvent::Bytes(b"callback bytes".to_vec())]
        );
        assert!(registry.remove(handle));
    }

    #[test]
    fn file_sink_writes_bytes_on_worker_thread() {
        let registry = TeeRegistry::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tee.log");
        let handle = registry
            .add_file(
                TeeStream::Stdout,
                &path,
                TeeFileOptions {
                    mode: TeeFileMode::Truncate,
                    queue_capacity: 4,
                    write_missed_markers: true,
                },
            )
            .expect("file sink");

        registry.write(TeeStream::Stdout, b"file bytes");

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let bytes = fs::read(&path).unwrap_or_default();
            if bytes == b"file bytes" {
                break;
            }
            if Instant::now() >= deadline {
                panic!("file sink did not write expected bytes, got {bytes:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(registry.remove(handle));
    }
}
