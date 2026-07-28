//! Cooperative all-thread stack capture (#635, S6).
//!
//! # Cooperative, not external
//!
//! The probe thread lives *inside* the target process and walks its own
//! sibling threads. There is no ptrace, no debugger attach, and no OS
//! capability grant — those belong to the later `--force` external tier.
//!
//! # The suspend window is the whole design
//!
//! A suspended thread may hold *any* lock, including the allocator's. So while
//! a thread is suspended this code does exactly two things: read its registers
//! and `memcpy` a bounded slice of its stack into a **preallocated** buffer.
//! Then it resumes immediately.
//!
//! Nothing else happens in that window — no allocation, no symbolization, no
//! logging, no lock acquisition. Unwinding and symbolization run afterward,
//! against the copied bytes, when every thread is running again. Violating
//! this is how a stack profiler deadlocks the process it is profiling: suspend
//! a thread inside `malloc`, then call `malloc` yourself.
//!
//! # Status
//!
//! This slice ships the OS-agnostic types and the **Windows** capture path.
//! Linux (realtime-signal context capture) and macOS (mach `thread_suspend`)
//! follow as separate changes, as does turning the captured registers and
//! stack bytes into return addresses. `Snapshot::frames_resolved` reports
//! whether that final step has run, so a consumer can never mistake raw
//! captures for unwound frames.

// x86_64 only. The capture reads `CONTEXT.Rsp`/`Rip`/`Rbp`, which are
// x86_64 register names; Windows on ARM64 uses `Sp`/`Pc`/`Fp` and a different
// unwind model. Same architecture gate the Windows interposer already carries.
// ARM64 support is a separate change, not a silently-wrong register read.
#[cfg(all(windows, target_arch = "x86_64"))]
pub mod modules;

#[cfg(all(windows, target_arch = "x86_64"))]
pub mod unwind;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;

use std::time::Duration;

/// Upper bound on the stack bytes copied per thread.
///
/// Bounded because the copy happens with the thread suspended: an unbounded
/// read would extend the window in proportion to stack depth. 256 KiB covers
/// realistic call depths while keeping the window short and the buffer
/// preallocatable.
pub const MAX_STACK_BYTES: usize = 256 * 1024;

/// How the capture was obtained, and what remains to be done to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// Registers plus raw stack bytes. Not yet unwound into return addresses.
    RawContext,
}

/// One thread's captured state.
#[derive(Clone, Debug)]
pub struct ThreadSample {
    /// OS thread id.
    pub os_tid: u64,
    /// Stack pointer at capture time.
    pub stack_pointer: u64,
    /// Instruction pointer at capture time.
    pub instruction_pointer: u64,
    /// Frame pointer at capture time.
    pub frame_pointer: u64,
    /// Bytes copied from the stack, starting at `stack_pointer`.
    pub stack_bytes: Vec<u8>,
    /// True when the stack was longer than [`MAX_STACK_BYTES`], so the copy is
    /// a prefix. A consumer must not read a truncated capture as a complete
    /// one.
    pub truncated: bool,
    /// What stage this sample is at.
    pub kind: CaptureKind,
    /// Return addresses, once unwinding has run. Empty until then — check
    /// [`Snapshot::frames_resolved`] rather than inferring from emptiness,
    /// since a thread with an unwalkable stack also yields none.
    pub frames: Vec<u64>,
}

/// What a capture cost and covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Sibling threads observed during enumeration.
    pub threads_total: u32,
    /// Threads successfully captured.
    pub threads_captured: u32,
    /// Threads that could not be captured (exited mid-capture, access denied).
    ///
    /// Non-zero means the snapshot is partial.
    pub threads_dropped: u32,
    /// Total time any thread spent suspended. The cost imposed on the target.
    pub pause_nanos: u64,
}

/// The result of one capture.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Per-thread samples.
    pub threads: Vec<ThreadSample>,
    /// Coverage and cost.
    pub stats: SnapshotStats,
    /// Whether `frames` have been resolved from the raw captures.
    ///
    /// False for now on every platform — unwinding lands separately. Exposed
    /// so a consumer cannot mistake raw register/stack captures for symbolized
    /// or even unwound frames.
    pub frames_resolved: bool,
}

impl Snapshot {
    /// Whether every enumerated thread was captured.
    pub fn is_complete(&self) -> bool {
        self.stats.threads_dropped == 0
    }

    /// Total time threads spent suspended.
    pub fn pause(&self) -> Duration {
        Duration::from_nanos(self.stats.pause_nanos)
    }
}

/// Knobs for a capture.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotConfig {
    /// Per-thread stack copy limit.
    pub max_stack_bytes: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_stack_bytes: MAX_STACK_BYTES,
        }
    }
}

/// Why a capture could not run at all.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// No capture backend for this platform yet.
    #[error("cooperative snapshot is not implemented for this platform yet")]
    Unsupported,
    /// The OS refused an enumeration or capture call.
    #[error("snapshot failed: {0}")]
    Os(#[from] std::io::Error),
}

/// Capture every sibling thread of the calling thread.
///
/// The calling thread is deliberately excluded: suspending yourself is an
/// immediate deadlock, and its stack is available directly anyway.
///
/// Returns [`SnapshotError::Unsupported`] on platforms whose backend has not
/// landed, rather than silently returning an empty snapshot that would read as
/// "this process has no threads".
pub fn capture_all_threads(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        windows::capture(config)
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = config;
        Err(SnapshotError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_the_documented_cap() {
        assert_eq!(SnapshotConfig::default().max_stack_bytes, MAX_STACK_BYTES);
    }

    #[test]
    fn a_snapshot_with_drops_is_not_complete() {
        let mut snap = Snapshot::default();
        assert!(snap.is_complete());
        snap.stats.threads_dropped = 1;
        assert!(
            !snap.is_complete(),
            "a dropped thread must make the snapshot partial"
        );
    }

    /// Raw captures must never be mistaken for unwound frames.
    #[test]
    fn raw_captures_report_frames_unresolved() {
        let snap = Snapshot::default();
        assert!(!snap.frames_resolved);
    }

    #[test]
    fn pause_is_reported_in_wall_clock_terms() {
        let snap = Snapshot {
            stats: SnapshotStats {
                pause_nanos: 1_500_000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(snap.pause(), Duration::from_micros(1500));
    }

    #[cfg(not(all(windows, target_arch = "x86_64")))]
    #[test]
    fn unimplemented_platforms_report_unsupported_not_empty() {
        // An empty Ok(Snapshot) would read as "no threads", which is a very
        // different claim from "not implemented here".
        assert!(matches!(
            capture_all_threads(&SnapshotConfig::default()),
            Err(SnapshotError::Unsupported)
        ));
    }
}
