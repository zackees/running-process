//! The actor that exclusively owns a [`NativeProcess`](crate::NativeProcess)'s
//! child (#850).
//!
//! Before this module the child lived in an `Arc<Mutex<Option<_>>>` that
//! `wait`, `kill`, `poll`, `pid` and the lifecycle observer all locked from
//! different threads, and the observer could only `try_lock` it on a timer.
//! Now one task on the process-global actor runtime owns the
//! [`PlatformStdChild`] -- and with it the Windows Job Object that contains
//! the tree -- and every operation on it is a command sent over a channel.
//! There is no shared child state to lock.
//!
//! The actor is the only writer of the exit: it publishes through
//! `SharedState::record_exit` exactly once, on the first `try_wait` that
//! reports one (from its own 10 ms lifecycle tick or from a command).
//!
//! # Lifetime
//!
//! The actor outlives the child's exit for as long as any [`ChildHandle`]
//! exists, then drops the child (and Job Object). Dropping the last handle
//! while the child still runs does not end the actor early: it keeps
//! observing until the exit, as the lifecycle task always did, so a dropped
//! wrapper never turns into an immediate kill-on-close of the job.
//!
//! # Blocking
//!
//! The actor never blocks: `try_wait` and the kill signal are non-blocking on
//! every host. Synchronous callers reach it through
//! [`actor_runtime::block_on_anywhere`], which is safe from any thread and
//! from inside any Tokio runtime.

use std::io;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant as TokioInstant;

use crate::{
    actor_runtime, child_try_wait_error_is_retryable, finalize_capture_completion_async,
    kill_drain_deadline, ChildState, SharedState,
};

/// How often the actor looks for a natural exit.
///
/// #199: intentional polling. `try_wait` is the only reap primitive shared by
/// standard and exact-trace children; 10 ms keeps the cost negligible while
/// staying responsive.
const LIFECYCLE_TICK: Duration = Duration::from_millis(10);

/// What a kill command found.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum KillOutcome {
    /// The child had already exited; nothing was signalled.
    AlreadyExited(i32),
    /// The kill signal was delivered; the exit is published asynchronously.
    Signalled,
}

enum ChildCommand {
    /// One non-blocking exit check.
    TryWait(oneshot::Sender<io::Result<Option<i32>>>),
    /// Hard-kill the child, or its whole group when it leads one.
    Kill(oneshot::Sender<io::Result<KillOutcome>>),
}

/// A cheap, cloneable handle to a running child actor: the command sender and
/// the child's pid, which is immutable and so is cached rather than asked for.
pub(crate) struct ChildHandle {
    commands: ManuallyDrop<mpsc::UnboundedSender<ChildCommand>>,
    pid: u32,
    /// The process image that owns the actor. A forked copy of this handle
    /// shares the channel but not the runtime that serves it, so a request
    /// would wait for a reply that can never come.
    owner: u32,
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // Dropping the last sender wakes the actor through its runtime. In a
        // forked copy that runtime has no workers and may hold a lock some
        // other thread owned at fork time, so the copy is leaked instead.
        if self.owner == std::process::id() {
            // SAFETY: dropped exactly once, here, and never used afterwards.
            unsafe { ManuallyDrop::drop(&mut self.commands) };
        }
    }
}

impl ChildHandle {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Non-blocking exit check served by the actor.
    pub(crate) fn try_wait(&self) -> io::Result<Option<i32>> {
        self.request(ChildCommand::TryWait)
    }

    /// Signal the child (or its group) unless it already exited.
    pub(crate) fn kill(&self) -> io::Result<KillOutcome> {
        self.request(ChildCommand::Kill)
    }

    fn request<T: Send + 'static>(
        &self,
        command: impl FnOnce(oneshot::Sender<io::Result<T>>) -> ChildCommand,
    ) -> io::Result<T> {
        if self.owner != std::process::id() {
            return Err(actor_gone());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| actor_gone())?;
        actor_runtime::block_on_anywhere(answer)
            .map_err(|_| actor_gone())
            .and_then(|result| result)
    }
}

fn actor_gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the child's lifecycle actor is not running in this process",
    )
}

/// Move `child` into a new actor on the process-global runtime.
pub(crate) fn spawn(
    child: ChildState,
    shared: Arc<SharedState>,
    capture: bool,
    capture_cancellation: Arc<
        running_process_platform_internal::platform::process::CaptureCancellation,
    >,
) -> ChildHandle {
    let (commands, inbox) = mpsc::unbounded_channel();
    let handle = ChildHandle {
        commands: ManuallyDrop::new(commands),
        pid: child.id(),
        owner: std::process::id(),
    };
    let actor = ChildActor {
        pid: handle.pid,
        child,
        shared,
        capture,
        capture_cancellation: Some(capture_cancellation),
        exited: None,
        observing: true,
    };
    actor_runtime::runtime().spawn(actor.run(inbox));
    handle
}

struct ChildActor {
    pid: u32,
    child: ChildState,
    shared: Arc<SharedState>,
    capture: bool,
    /// Handed to the post-exit drain task, so the cancellation state is not
    /// kept alive by the actor once its work is done.
    capture_cancellation:
        Option<Arc<running_process_platform_internal::platform::process::CaptureCancellation>>,
    /// The published exit; once set it is never written again.
    exited: Option<i32>,
    /// Cleared by a terminal `try_wait` error: the tick stops, commands keep
    /// reporting the error to whoever asks.
    observing: bool,
}

impl ChildActor {
    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<ChildCommand>) {
        let mut next_tick = TokioInstant::now() + LIFECYCLE_TICK;
        loop {
            let ticking = self.exited.is_none() && self.observing;
            let received = if ticking {
                tokio::time::timeout_at(next_tick, inbox.recv()).await.ok()
            } else {
                Some(inbox.recv().await)
            };
            match received {
                Some(Some(command)) => self.serve(command),
                // Every handle is gone. A child that is still running is
                // observed to its exit (the job must outlive it); after that
                // there is nothing left to own.
                Some(None) => return self.drain_until_exit(next_tick).await,
                None => {}
            }
            if ticking && TokioInstant::now() >= next_tick {
                self.observe();
                // Delay, not burst: a late tick does not queue catch-up ones.
                next_tick = TokioInstant::now() + LIFECYCLE_TICK;
            }
        }
    }

    /// Keep the lifecycle tick going with no commands left to serve.
    async fn drain_until_exit(mut self, mut next_tick: TokioInstant) {
        while self.exited.is_none() && self.observing {
            tokio::time::sleep_until(next_tick).await;
            self.observe();
            next_tick = TokioInstant::now() + LIFECYCLE_TICK;
        }
    }

    fn serve(&mut self, command: ChildCommand) {
        match command {
            ChildCommand::TryWait(reply) => {
                let _ = reply.send(self.check());
            }
            ChildCommand::Kill(reply) => {
                let _ = reply.send(self.kill());
            }
        }
    }

    /// The lifecycle tick: look for an exit, tolerating a transient error.
    fn observe(&mut self) {
        match self.check() {
            Ok(_) => {}
            Err(error) if child_try_wait_error_is_retryable(&error) => {}
            Err(_) => self.observing = false,
        }
    }

    /// One non-blocking exit check that publishes the exit if it is the one
    /// that finds it.
    fn check(&mut self) -> io::Result<Option<i32>> {
        if let Some(code) = self.exited {
            return Ok(Some(code));
        }
        let status = self.child.try_wait_code()?;
        if let Some(code) = status {
            self.publish(code);
        }
        Ok(status)
    }

    fn kill(&mut self) -> io::Result<KillOutcome> {
        if let Some(code) = self.check()? {
            return Ok(KillOutcome::AlreadyExited(code));
        }
        // Group-wide when the child leads its own group
        // (`create_process_group`) and the host can signal groups, otherwise
        // the direct child.
        self.child.kill_group_or_child()?;
        Ok(KillOutcome::Signalled)
    }

    /// Publish the exit to every observer, exactly once.
    fn publish(&mut self, code: i32) {
        debug_assert!(self.exited.is_none(), "exit published twice");
        self.exited = Some(code);
        self.shared.record_exit(code);
        // Phase 1 of #221: lifecycle `exited`, guarded so it fires once.
        self.shared.emit_exited(self.pid, code);
        let cancellation = self.capture_cancellation.take();
        if let (true, Some(cancellation)) = (self.capture, cancellation) {
            // The direct child has exited. Bound the capture-completion wait
            // so wait()/close()/read_* on the natural-exit path cannot wedge
            // forever when a grandchild inherited the pipe and outlives the
            // child (issue #590, cluster A). The reader is not cancelled up
            // front: a short-lived grandchild may still emit output the
            // caller expects to capture, so it drains within the grace
            // window. Only if the window elapses with the pipe still held
            // open is it cancelled, to release the leaked reader thread. This
            // runs as its own task so the actor keeps serving commands.
            let shared = Arc::clone(&self.shared);
            let deadline = kill_drain_deadline();
            actor_runtime::runtime().spawn(async move {
                if !finalize_capture_completion_async(&shared, deadline).await {
                    running_process_platform_internal::platform::process::cancel_capture_reader(
                        &cancellation,
                    );
                }
            });
        }
        // Non-invasive watch EOF is owned by the platform descendant backend:
        // Linux/macOS perform one final reconciliation and Windows waits for
        // ACTIVE_PROCESS_ZERO. Closing here would race those final
        // descendant notifications.
    }
}

#[cfg(test)]
mod tests {
    use std::mem::ManuallyDrop;

    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TryRecvError;

    use super::{ChildCommand, ChildHandle};

    /// A handle as a forked copy sees it: same channel, but owned by another
    /// process image than the current one.
    fn forked_copy() -> (ChildHandle, mpsc::UnboundedReceiver<ChildCommand>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let handle = ChildHandle {
            commands: ManuallyDrop::new(commands),
            pid: 4242,
            owner: std::process::id().wrapping_add(1),
        };
        (handle, inbox)
    }

    #[test]
    fn a_forked_copy_fails_fast_instead_of_waiting_for_a_runtime_it_lacks() {
        let (handle, mut inbox) = forked_copy();
        assert_eq!(handle.pid(), 4242, "the cached pid needs no actor");
        let error = handle.try_wait().expect_err("no actor serves this image");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(handle.kill().is_err());
        assert!(
            matches!(inbox.try_recv(), Err(TryRecvError::Empty)),
            "a request must not even be queued for the foreign actor"
        );
    }

    #[test]
    fn dropping_a_forked_copy_does_not_close_the_actors_channel() {
        let (handle, mut inbox) = forked_copy();
        drop(handle);
        // Closing the channel is what would wake the actor through its
        // runtime; a forked copy must leave it alone.
        assert!(matches!(inbox.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dropping_the_owning_handle_closes_the_actors_channel() {
        let (commands, mut inbox) = mpsc::unbounded_channel();
        let handle = ChildHandle {
            commands: ManuallyDrop::new(commands),
            pid: 1,
            owner: std::process::id(),
        };
        drop(handle);
        assert!(matches!(inbox.try_recv(), Err(TryRecvError::Disconnected)));
    }
}
