use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{NativePtyProcess, PtyError};

/// Broker-ready owner for a PTY process that can outlive foreground clients.
///
/// `NativePtyProcess` remains the low-level process/PTY primitive. This wrapper
/// gives callers an explicit session/attachment split: the session owns the PTY
/// lifetime, while attachments provide temporary foreground access to read,
/// write, resize, or interrupt the PTY. Dropping or detaching an attachment does
/// not kill the child.
#[derive(Clone)]
pub struct DetachablePtySession {
    process: Arc<NativePtyProcess>,
    attached: Arc<AtomicBool>,
}

impl DetachablePtySession {
    /// Start a PTY process and return a detachable session owner.
    pub fn spawn(process: NativePtyProcess) -> Result<Self, PtyError> {
        process.start_impl()?;
        Ok(Self::from_started(process))
    }

    /// Wrap an already-started PTY process as a detachable session owner.
    pub fn from_started(process: NativePtyProcess) -> Self {
        Self {
            process: Arc::new(process),
            attached: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn process(&self) -> &NativePtyProcess {
        &self.process
    }

    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Acquire)
    }

    /// Attach a foreground client. Only one attachment is active at a time.
    pub fn attach(&self) -> Result<DetachablePtyAttachment, PtyError> {
        self.attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PtyError::Other("detachable PTY already has an attachment".into()))?;
        Ok(DetachablePtyAttachment {
            process: Arc::clone(&self.process),
            attached: Arc::clone(&self.attached),
            detached: false,
        })
    }

    pub fn wait(&self, timeout: Option<f64>) -> Result<i32, PtyError> {
        self.process.wait_impl(timeout)
    }

    pub fn terminate_tree(&self) -> Result<(), PtyError> {
        self.process.terminate_tree_impl()
    }

    pub fn kill_tree(&self) -> Result<(), PtyError> {
        self.process.kill_tree_impl()
    }

    pub fn close(&self) -> Result<(), PtyError> {
        self.process.close_impl()
    }
}

/// Temporary foreground access to a detachable PTY session.
///
/// Dropping this value detaches the foreground client but leaves the owning
/// `DetachablePtySession` and child process alive.
pub struct DetachablePtyAttachment {
    process: Arc<NativePtyProcess>,
    attached: Arc<AtomicBool>,
    detached: bool,
}

impl DetachablePtyAttachment {
    pub fn read_chunk(&self, timeout: Option<f64>) -> Result<Option<Vec<u8>>, PtyError> {
        self.process.read_chunk_impl(timeout)
    }

    pub fn write(&self, data: &[u8], submit: bool) -> Result<(), PtyError> {
        self.process.write_impl(data, submit)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.process.resize_impl(rows, cols)
    }

    pub fn send_interrupt(&self) -> Result<(), PtyError> {
        self.process.send_interrupt_impl()
    }

    /// Detach this foreground client without terminating the PTY child.
    pub fn detach(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.detached {
            self.attached.store(false, Ordering::Release);
            self.detached = true;
        }
    }
}

impl Drop for DetachablePtyAttachment {
    fn drop(&mut self) {
        self.release();
    }
}
