//! Shared deadline-bounded I/O for accepted broker control connections.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

const CONTROL_IO_POLL_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_HELLO_READ_TIMEOUT: Duration = Duration::from_secs(30);
const HELLO_READ_TIMEOUT_ENV: &str = "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS";

#[doc(hidden)]
pub fn hello_read_deadline() -> Instant {
    let timeout = std::env::var(HELLO_READ_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_HELLO_READ_TIMEOUT);
    Instant::now() + timeout
}

/// Wraps a nonblocking broker stream and bounds I/O against a deadline.
#[doc(hidden)]
pub struct DeadlineStream<'a, S> {
    inner: &'a mut S,
    deadline: Instant,
}

impl<'a, S> DeadlineStream<'a, S> {
    pub fn new(inner: &'a mut S, deadline: Instant) -> Self {
        Self { inner, deadline }
    }

    fn wait(&self) -> std::io::Result<()> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for broker peer to send a complete frame",
            ));
        }
        std::thread::sleep((self.deadline - now).min(CONTROL_IO_POLL_INTERVAL));
        Ok(())
    }
}

impl<S: Read> Read for DeadlineStream<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                // Windows `PIPE_NOWAIT` reports an empty pipe as `Ok(0)`;
                // Unix only returns zero for EOF, which framing must keep
                // distinct from a deadline expiry.
                Ok(0) if cfg!(windows) => self.wait()?,
                Ok(0) => return Ok(0),
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl<S: Write> Write for DeadlineStream<'_, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.write(buf) {
                Ok(0) => self.wait()?,
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        loop {
            match self.inner.flush() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn eof_remains_distinct_from_timeout() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let mut stream = DeadlineStream::new(&mut empty, Instant::now() + Duration::from_secs(1));
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).expect("EOF is not an error"), 0);
    }
}
