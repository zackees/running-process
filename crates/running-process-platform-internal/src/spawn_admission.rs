//! Caller-owned exclusion acquired at the synchronous native spawn boundary.

use std::{any::Any, fmt, io, sync::Arc};

/// Admission callback shared by cloned spawn descriptions.
///
/// Acquisition and permit destruction happen on the native spawning thread,
/// around process creation, never in a post-fork callback or across an await.
/// The permit may be non-Send (for example, a static RwLock read guard).
/// Callbacks may block that thread and must not await or depend on progress
/// from the same process actor. Denial prevents process creation.
#[derive(Clone)]
pub struct SpawnAdmission {
    acquire: Arc<dyn Fn() -> io::Result<Box<dyn Any>> + Send + Sync>,
}

impl fmt::Debug for SpawnAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SpawnAdmission { .. }")
    }
}

impl SpawnAdmission {
    /// Capture an acquisition function; its returned permit is dropped after
    /// native spawn succeeds or fails. The permit must own its borrow lifetime.
    pub fn new<F, G>(acquire: F) -> Self
    where
        F: Fn() -> io::Result<G> + Send + Sync + 'static,
        G: 'static,
    {
        Self {
            acquire: Arc::new(move || acquire().map(|guard| Box::new(guard) as Box<dyn Any>)),
        }
    }

    pub(crate) fn run<T>(&self, spawn: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let _permit = (self.acquire)()?;
        spawn()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn non_send_permit_is_dropped_on_every_spawn_exit() {
        struct Permit(Arc<AtomicBool>, std::rc::Rc<()>);
        impl Drop for Permit {
            fn drop(&mut self) {
                let _ = &self.1;
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let held = Arc::new(AtomicBool::new(false));
        let marker = held.clone();
        let admission = SpawnAdmission::new(move || {
            marker.store(true, Ordering::SeqCst);
            Ok(Permit(marker.clone(), std::rc::Rc::new(())))
        });
        let result: io::Result<()> = admission.run(|| {
            assert!(held.load(Ordering::SeqCst));
            Err(io::ErrorKind::NotFound.into())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!held.load(Ordering::SeqCst));

        let value = admission
            .clone()
            .run(|| {
                assert!(held.load(Ordering::SeqCst));
                Ok(37)
            })
            .unwrap();
        assert_eq!(value, 37);
        assert!(!held.load(Ordering::SeqCst));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: io::Result<()> = admission.run(|| {
                assert!(held.load(Ordering::SeqCst));
                panic!("fixture panic before process creation");
            });
        }));
        assert!(panic.is_err());
        assert!(
            !held.load(Ordering::SeqCst),
            "unwinding must release admission"
        );
    }

    #[test]
    fn denial_never_invokes_spawn() {
        let admission =
            SpawnAdmission::new(|| Err::<(), _>(io::ErrorKind::PermissionDenied.into()));
        let result: io::Result<()> = admission.run(|| panic!("denied spawn must not execute"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
