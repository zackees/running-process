//! Frozen v1 manifest and service-definition registration substrate.
//!
//! The public modules here are the canonical owner of persisted registration.
//! Legacy client-only `broker` paths re-export these exact items for source and
//! type identity compatibility. Endpoint names, connection policy, client
//! transport, and daemon runtime deliberately stay elsewhere.

/// Narrow set of generated v1 messages needed to construct, persist, and
/// inspect daemon registration records.
pub mod protocol {
    pub use running_process_protocol::broker::v1::{
        BrokerIsolation, CacheManifest, CacheRoot, CacheRootKind, CleanupPolicy, DaemonProcess,
        Endpoint, HostIdentity, ManifestRef, ObservabilityInfo, Operation, OperationKind,
        Ownership, Quota, ServiceDefinition, StorageDisposition, TeardownHook, TeardownKind,
    };
}

/// Name/version validation shared by registration and the legacy v1 broker.
pub mod validation {
    pub use crate::daemon_registration_common::validation::*;
}

/// Host facts stamped into a newly-built manifest.
pub mod host_identity {
    pub use crate::daemon_host_identity::{current, current_for_path};
}

pub(crate) use crate::daemon_registration_common::secure_dir;

/// SHA-256 sealed v1 manifest persistence, default paths, and scanning.
#[path = "broker/manifest.rs"]
pub mod manifest;

/// Validated v1 `.servicedef` persistence and loading.
#[path = "broker/server/service_def_loader.rs"]
pub mod service_def_loader;

/// Fluent builders for the frozen v1 registration messages.
#[path = "broker/builders.rs"]
pub mod builders;

#[cfg(all(test, feature = "client"))]
mod compatibility_tests {
    use std::any::TypeId;

    #[test]
    fn legacy_client_registration_paths_reexport_canonical_types() {
        assert_eq!(
            TypeId::of::<crate::daemon_registration::protocol::CacheManifest>(),
            TypeId::of::<crate::broker::protocol::CacheManifest>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::protocol::ServiceDefinition>(),
            TypeId::of::<crate::broker::protocol::ServiceDefinition>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::protocol::HostIdentity>(),
            TypeId::of::<crate::broker::protocol::HostIdentity>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::builders::CacheManifestBuilder>(),
            TypeId::of::<crate::broker::builders::CacheManifestBuilder>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::builders::ServiceDefinitionBuilder>(),
            TypeId::of::<crate::broker::builders::ServiceDefinitionBuilder>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::manifest::ManifestError>(),
            TypeId::of::<crate::broker::manifest::ManifestError>(),
        );
        assert_eq!(
            TypeId::of::<crate::daemon_registration::service_def_loader::ServiceDefinitionError>(),
            TypeId::of::<crate::broker::server::service_def_loader::ServiceDefinitionError>(),
        );
    }
}
