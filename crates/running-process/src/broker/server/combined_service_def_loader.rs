//! v2-first service-definition lookup for the shared (v2) broker serve path.
//!
//! soldr#2364. `serve_launching_backends` / [`super::HelloRouter`] historically
//! read only v1 `.servicedef` files, but soldr — and every consumer that
//! installs via [`crate::broker::protocol_v2::write_service_definition_v2`] —
//! writes `.servicedef.v2` files. So a real Hello against the shared broker was
//! `Refused { "service definition was not found" }`: the file was on disk under
//! the v2 extension the v1 loader never looks for.
//!
//! This module closes that gap without disturbing the downstream routing chain.
//! [`CombinedServiceDefinitionLoader`] tries the v2 file first, falls back to
//! the v1 file, and returns the **v1** [`ServiceDefinition`] type that
//! `check_version_allowed`, [`super::BrokerInstanceKey::from_service_definition`],
//! `BackendLaunchRequest`, and the rest of the launch path already consume. The
//! v2 schema is a strict superset of v1 (identical field numbers 1–8, plus the
//! v2-only optional `http_server` capability at field 10), so the
//! down-conversion is lossless for everything the broker launch path reads.
//!
//! [`HelloRouter`] takes its loader as a `&dyn `[`ServiceDefinitionSource`] so
//! both the bare v1 [`ServiceDefinitionLoader`] (used by the crate's existing
//! unit/integration tests, which install v1 files) and this combined loader
//! satisfy it: a `&ServiceDefinitionLoader` unsize-coerces to the trait object
//! at the call site, so those call sites need no change.

use crate::broker::protocol::ServiceDefinition;
use crate::broker::protocol_v2;
use crate::broker::server::service_def_loader::{ServiceDefinitionError, ServiceDefinitionLoader};
use std::path::{Path, PathBuf};

/// A source of v1-typed service definitions for [`super::HelloRouter`].
///
/// Implemented by the bare v1 [`ServiceDefinitionLoader`] and by
/// [`CombinedServiceDefinitionLoader`]. The router only ever needs to look a
/// service up by name, so this is the whole surface.
pub trait ServiceDefinitionSource {
    /// Look up (always re-reading from disk) the service definition for
    /// `service_name`, returning the v1 [`ServiceDefinition`] type.
    fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError>;
}

impl ServiceDefinitionSource for ServiceDefinitionLoader {
    fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        ServiceDefinitionLoader::lookup_or_reload(self, service_name)
    }
}

/// Reads `.servicedef.v2` first, then `.servicedef`, returning the v1
/// [`ServiceDefinition`] the broker routing chain consumes.
#[derive(Clone)]
pub struct CombinedServiceDefinitionLoader {
    root: PathBuf,
}

impl CombinedServiceDefinitionLoader {
    /// Build a loader rooted at `root` (the service-definition directory).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The service-definition directory this loader reads from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load and validate one service definition, preferring the v2 file.
    ///
    /// A missing v2 file (only) falls back to the v1 file; any other v2 error
    /// (insecure directory, decode failure, name mismatch) is returned as-is so
    /// a genuinely broken v2 file is never silently masked by a v1 fallback.
    pub fn load(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        match protocol_v2::ServiceDefinitionLoader::new(&self.root).load(service_name) {
            Ok(v2) => Ok(service_definition_v2_to_v1(v2)),
            Err(err) if is_missing_file(&err) => {
                ServiceDefinitionLoader::new(&self.root).load(service_name)
            }
            Err(err) => Err(err),
        }
    }

    /// Re-read one service definition from disk (alias for [`Self::load`]).
    pub fn reload(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }

    /// Lookup that always re-reads — mirrors the v1 loader's contract.
    pub fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }
}

impl ServiceDefinitionSource for CombinedServiceDefinitionLoader {
    fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        CombinedServiceDefinitionLoader::lookup_or_reload(self, service_name)
    }
}

/// True when `err` is a plain "file not found" — the only v2 error that should
/// trigger a v1 fallback rather than surface.
fn is_missing_file(err: &ServiceDefinitionError) -> bool {
    matches!(err, ServiceDefinitionError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

/// Down-convert a v2 [`protocol_v2::ServiceDefinition`] to the v1
/// [`ServiceDefinition`] the broker launch chain consumes.
///
/// Fields 1–8 are identical across the two schemas (same field numbers, same
/// wire types; `isolation` is a plain `i32` in both, with matching
/// `BrokerIsolation` discriminants). The v2-only `http_server` capability
/// (field 10) has no v1 equivalent and is dropped — the broker launch path
/// never reads it (it is an aggregator-facing HTTP hint, not launch policy).
pub fn service_definition_v2_to_v1(v2: protocol_v2::ServiceDefinition) -> ServiceDefinition {
    ServiceDefinition {
        service_name: v2.service_name,
        binary_path: v2.binary_path,
        isolation: v2.isolation,
        explicit_instance: v2.explicit_instance,
        per_version_binary_dir: v2.per_version_binary_dir,
        min_version: v2.min_version,
        version_allow_list: v2.version_allow_list,
        labels: v2.labels,
    }
}

#[cfg(test)]
mod tests;
