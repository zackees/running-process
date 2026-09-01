//! Legacy client-path aliases for canonical v1 manifest persistence.

#[cfg(feature = "client")]
pub(crate) use crate::daemon_registration::manifest::write_atomic;
pub use crate::daemon_registration::manifest::*;
