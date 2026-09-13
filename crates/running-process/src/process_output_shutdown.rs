//! Sticky output-cleanup request and separately acknowledged pump completion.

use super::*;

type Completion = Option<Result<(), SessionExitError>>;

#[derive(Clone)]
pub(crate) struct SessionOutputShutdown {
    request: watch::Sender<bool>,
    completion: watch::Receiver<Completion>,
    #[cfg(test)]
    checkpoint: Arc<std::sync::Mutex<Option<Arc<CleanupCheckpoint>>>>,
    #[cfg(test)]
    panic_next_poll: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) struct SessionOutputProducer {
    pub(super) events: mpsc::Sender<AsyncProcessSessionEvent>,
    pub(super) shutdown: SessionOutputShutdown,
    pub(super) completion: watch::Sender<Completion>,
}

impl SessionOutputShutdown {
    pub(super) fn new(
        events: mpsc::Sender<AsyncProcessSessionEvent>,
    ) -> (Self, SessionOutputProducer) {
        let (request, _) = watch::channel(false);
        let (completion_tx, completion) = watch::channel(None);
        let state = Self {
            request,
            completion,
            #[cfg(test)]
            checkpoint: Arc::default(),
            #[cfg(test)]
            panic_next_poll: Arc::default(),
        };
        let producer = SessionOutputProducer {
            events,
            shutdown: state.clone(),
            completion: completion_tx,
        };
        (state, producer)
    }

    pub(crate) fn request(&self) {
        self.request.send_replace(true);
    }

    pub(super) async fn requested(&self) {
        let mut request = self.request.subscribe();
        loop {
            if *request.borrow_and_update() {
                return;
            }
            if request.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) async fn wait(&self) -> Result<(), ProcessError> {
        let mut completion = self.completion.clone();
        loop {
            let state = completion.borrow_and_update().clone();
            if let Some(result) = state {
                return result.map_err(|error| ProcessError::Io(error.into_io()));
            }
            completion
                .changed()
                .await
                .map_err(|_| ProcessError::NotRunning)?;
        }
    }

    pub(super) fn guard(&self) -> ShutdownOnDrop {
        ShutdownOnDrop(self.clone())
    }

    #[cfg(test)]
    pub(crate) fn inject_pump_panic(&self) {
        self.panic_next_poll.store(true, Ordering::SeqCst);
        self.request.send_replace(false);
    }

    #[cfg(test)]
    pub(super) fn maybe_panic(&self) {
        assert!(
            !self.panic_next_poll.swap(false, Ordering::SeqCst),
            "injected pump panic"
        );
    }

    #[cfg(test)]
    pub(crate) fn hold_cleanup(&self) -> CleanupCheckpointGuard {
        let checkpoint = Arc::new(CleanupCheckpoint {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        *self.checkpoint.lock().expect("checkpoint mutex") = Some(checkpoint.clone());
        CleanupCheckpointGuard(checkpoint)
    }

    #[cfg(test)]
    pub(super) async fn pause_before_cleanup(&self) {
        let checkpoint = self.checkpoint.lock().expect("checkpoint mutex").clone();
        if let Some(checkpoint) = checkpoint {
            checkpoint.entered.add_permits(1);
            let _ = checkpoint.release.acquire().await;
        }
    }
}

pub(super) struct ShutdownOnDrop(SessionOutputShutdown);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.0.request();
    }
}

#[cfg(test)]
struct CleanupCheckpoint {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
pub(crate) struct CleanupCheckpointGuard(Arc<CleanupCheckpoint>);

#[cfg(test)]
impl CleanupCheckpointGuard {
    pub(crate) async fn entered(&self, pumps: u32) {
        let _ = self
            .0
            .entered
            .acquire_many(pumps)
            .await
            .expect("checkpoint entry");
    }
}

#[cfg(test)]
impl Drop for CleanupCheckpointGuard {
    fn drop(&mut self) {
        self.0.release.close();
    }
}
