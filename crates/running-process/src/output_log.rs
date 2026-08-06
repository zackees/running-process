//! Bounded append-only output retention and independent reader cursors.
//!
//! The actor/output pumps can use this small policy object without coupling
//! retention to a particular transport. Consumers never remove records from
//! the log; each [`OutputCursor`] owns its own position and receives an
//! explicit [`CursorRead::Gap`] when retention has moved past it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::StreamKind;

/// One sequenced output observation retained by an [`OutputLog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRecord {
    /// Monotonic observation sequence assigned by the log.
    pub sequence: u64,
    /// Source stream that produced the bytes.
    pub stream: StreamKind,
    /// Raw bytes observed from the source stream.
    pub bytes: Vec<u8>,
}

/// Result of advancing an independent output cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorRead {
    /// Records before `to` are no longer retained and were explicitly lost.
    Gap {
        /// First sequence that was unavailable to this cursor.
        from: u64,
        /// Last sequence known to be unavailable before the retained window.
        to: u64,
    },
    /// One retained output record.
    Record(OutputRecord),
    /// No record is currently available.
    Eof,
}

/// A byte-bounded append-only output log.
#[derive(Debug)]
pub struct OutputLog {
    capacity_bytes: usize,
    retained_bytes: usize,
    next_sequence: u64,
    records: VecDeque<OutputRecord>,
}

impl OutputLog {
    /// Create a log with a fixed aggregate byte capacity.
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            retained_bytes: 0,
            next_sequence: 0,
            records: VecDeque::new(),
        }
    }

    /// Append bytes and return their assigned sequence number.
    ///
    /// Records that do not fit are not retained. Their sequence is still
    /// consumed, allowing existing cursors to report a precise gap.
    pub fn append(&mut self, stream: StreamKind, bytes: impl Into<Vec<u8>>) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let bytes = bytes.into();
        if bytes.len() > self.capacity_bytes {
            self.records.clear();
            self.retained_bytes = 0;
            return sequence;
        }

        self.retained_bytes = self.retained_bytes.saturating_add(bytes.len());
        self.records.push_back(OutputRecord {
            sequence,
            stream,
            bytes,
        });
        while self.retained_bytes > self.capacity_bytes {
            if let Some(record) = self.records.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(record.bytes.len());
            }
        }
        sequence
    }

    /// Return the first sequence still retained, or the next sequence when empty.
    pub fn first_sequence(&self) -> u64 {
        self.records
            .front()
            .map_or(self.next_sequence, |record| record.sequence)
    }

    /// Return the next sequence that will be assigned.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Return the number of bytes currently retained.
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Return the number of records currently retained.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log contains no retained records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Create a cursor positioned at the oldest currently retained record.
    pub fn cursor(&self) -> OutputCursor {
        OutputCursor {
            next_sequence: self.first_sequence(),
        }
    }

    /// Create a cursor positioned at an explicit sequence.
    pub fn cursor_from(&self, sequence: u64) -> OutputCursor {
        OutputCursor {
            next_sequence: sequence,
        }
    }
}

/// Independent position in an [`OutputLog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCursor {
    next_sequence: u64,
}

/// Thread-safe output-log handle suitable for sharing with actor consumers.
#[derive(Debug, Clone)]
pub struct SharedOutputLog {
    inner: Arc<Mutex<OutputLog>>,
}

impl SharedOutputLog {
    /// Create a shared log with a fixed aggregate byte capacity.
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputLog::new(capacity_bytes))),
        }
    }

    /// Append an observation without exposing the log's synchronization primitive.
    pub fn append(&self, stream: StreamKind, bytes: impl Into<Vec<u8>>) -> u64 {
        self.inner
            .lock()
            .expect("output log lock is not poisoned")
            .append(stream, bytes)
    }

    /// Create a cursor at the current retention boundary.
    pub fn cursor(&self) -> SharedOutputCursor {
        let cursor = self
            .inner
            .lock()
            .expect("output log lock is not poisoned")
            .cursor();
        SharedOutputCursor {
            inner: Arc::clone(&self.inner),
            cursor,
        }
    }

    /// Return the number of bytes currently retained.
    pub fn retained_bytes(&self) -> usize {
        self.inner
            .lock()
            .expect("output log lock is not poisoned")
            .retained_bytes()
    }
}

/// Independent cursor over a [`SharedOutputLog`].
#[derive(Debug, Clone)]
pub struct SharedOutputCursor {
    inner: Arc<Mutex<OutputLog>>,
    cursor: OutputCursor,
}

impl SharedOutputCursor {
    /// Read the next record or explicit gap from the shared log.
    pub fn read_next(&mut self) -> CursorRead {
        self.cursor
            .read_next(&self.inner.lock().expect("output log lock is not poisoned"))
    }

    /// Return the next sequence this cursor will request.
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }
}

impl OutputCursor {
    /// Return the next sequence this cursor will request.
    pub fn position(&self) -> u64 {
        self.next_sequence
    }

    /// Advance this cursor without consuming records for any other cursor.
    pub fn read_next(&mut self, log: &OutputLog) -> CursorRead {
        let first = log.first_sequence();
        if self.next_sequence < first {
            let from = self.next_sequence;
            self.next_sequence = first;
            return CursorRead::Gap {
                from,
                to: first.saturating_sub(1),
            };
        }
        let Some(record) = log
            .records
            .iter()
            .find(|record| record.sequence == self.next_sequence)
        else {
            return CursorRead::Eof;
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        CursorRead::Record(record.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorRead, OutputLog, SharedOutputLog};
    use crate::StreamKind;

    #[test]
    fn bounded_retention_reports_gaps_to_lagging_cursors() {
        let mut log = OutputLog::new(4);
        let mut cursor = log.cursor();
        assert_eq!(log.append(StreamKind::Stdout, b"aa"), 0);
        assert!(
            matches!(cursor.read_next(&log), CursorRead::Record(record) if record.sequence == 0)
        );
        log.append(StreamKind::Stderr, b"bb");
        log.append(StreamKind::Stdout, b"cc");
        log.append(StreamKind::Stderr, b"dd");
        assert_eq!(cursor.read_next(&log), CursorRead::Gap { from: 1, to: 1 });
        assert!(
            matches!(cursor.read_next(&log), CursorRead::Record(record) if record.sequence == 2)
        );
    }

    #[test]
    fn cursors_are_independent_and_oversized_records_are_explicitly_lost() {
        let mut log = OutputLog::new(3);
        log.append(StreamKind::Stdout, b"1234");
        let mut first = log.cursor_from(0);
        let mut second = log.cursor_from(0);
        log.append(StreamKind::Stderr, b"ok");
        assert!(matches!(
            first.read_next(&log),
            CursorRead::Gap { from: 0, to: 0 }
        ));
        assert!(matches!(
            second.read_next(&log),
            CursorRead::Gap { from: 0, to: 0 }
        ));
        assert!(
            matches!(first.read_next(&log), CursorRead::Record(record) if record.bytes == b"ok")
        );
        assert!(
            matches!(second.read_next(&log), CursorRead::Record(record) if record.bytes == b"ok")
        );
        assert_eq!(log.retained_bytes(), 2);
    }

    #[test]
    fn shared_log_keeps_cursor_positions_independent() {
        let log = SharedOutputLog::new(8);
        let mut first = log.cursor();
        let mut second = log.cursor();
        log.append(StreamKind::Stdout, b"one");
        assert!(matches!(first.read_next(), CursorRead::Record(_)));
        assert!(matches!(second.read_next(), CursorRead::Record(_)));
        assert_eq!(first.position(), second.position());
        assert_eq!(log.retained_bytes(), 3);
    }
}
