//! Caller-owned exclusion at the native spawn boundary.

use std::{any::Any, fmt, io, sync::Arc};

/// Admission callback shared by cloned spawn descriptions.
///
/// Its permit covers only native process creation and is dropped before the
/// caller can observe the child. The permit may be non-`Send`.
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
    /// Capture an admission function. Its returned permit is always dropped
    /// after the native spawn attempt, including when that attempt fails.
    pub fn new<F, G>(acquire: F) -> Self
    where
        F: Fn() -> io::Result<G> + Send + Sync + 'static,
        G: 'static,
    {
        Self {
            acquire: Arc::new(move || acquire().map(|permit| Box::new(permit) as Box<dyn Any>)),
        }
    }

    pub(crate) fn run<T>(&self, spawn: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let _permit = (self.acquire)()?;
        spawn()
    }
}
