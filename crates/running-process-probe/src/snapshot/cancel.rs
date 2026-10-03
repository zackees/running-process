//! Bounded file reads which can discard a snapshot when its sampler stops.
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};

const CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn read_to_end(
    mut reader: impl Read,
    stop: &AtomicBool,
    max_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    let mut chunk = [0; CHUNK_BYTES];
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        let limit = max_bytes
            .saturating_sub(data.len())
            .saturating_add(1)
            .min(CHUNK_BYTES);
        let count = match reader.read(&mut chunk[..limit]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        if count == 0 {
            return Ok(Some(data));
        }
        if count > max_bytes.saturating_sub(data.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "module image exceeds limit",
            ));
        }
        data.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StopAfterChunk<'a> {
        stop: &'a AtomicBool,
        reads: usize,
    }

    impl Read for StopAfterChunk<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads > 1 {
                panic!("cancelled image reader must not perform another read");
            }
            buffer.fill(7);
            self.stop.store(true, Ordering::Release);
            Ok(buffer.len())
        }
    }

    #[test]
    fn shutdown_during_a_chunk_discards_partial_image_and_stops_reading() {
        let stop = AtomicBool::new(false);
        let mut reader = StopAfterChunk {
            stop: &stop,
            reads: 0,
        };
        assert!(read_to_end(&mut reader, &stop, CHUNK_BYTES * 4)
            .unwrap()
            .is_none());
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn shutdown_before_read_never_touches_the_image() {
        let stop = AtomicBool::new(true);
        let mut reader = StopAfterChunk {
            stop: &stop,
            reads: 0,
        };
        assert!(read_to_end(&mut reader, &stop, CHUNK_BYTES)
            .unwrap()
            .is_none());
        assert_eq!(reader.reads, 0);
    }

    struct InterruptedOnce {
        interrupted: bool,
        bytes: &'static [u8],
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.bytes.read(buffer)
        }
    }

    #[test]
    fn interrupted_read_is_retried_without_losing_bytes() {
        let stop = AtomicBool::new(false);
        let reader = InterruptedOnce {
            interrupted: false,
            bytes: b"image",
        };
        assert_eq!(
            read_to_end(reader, &stop, 5).unwrap(),
            Some(b"image".to_vec())
        );
    }

    #[test]
    fn active_reader_preserves_bytes_and_enforces_size_limit() {
        let stop = AtomicBool::new(false);
        assert_eq!(
            read_to_end(&b"image"[..], &stop, 5).unwrap(),
            Some(b"image".to_vec())
        );
        assert_eq!(
            read_to_end(&b"image"[..], &stop, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
