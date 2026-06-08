//! Broker server foundation for `running-process-broker-v1`.
//!
//! Phase 4 (#235) grows this module into the pipe accept loop, service
//! definition loader, backend registry, admin verbs, and perf guard.
//! The first slice keeps the core Hello validation and negotiation
//! logic testable without binding sockets or spawning backends.

pub mod hello_handler;
pub mod service_def_loader;
pub mod version_allow_list;

pub use hello_handler::{
    HelloHandler, HelloHandlerError, HelloRequest, PeerIdentity, RegisteredBackend,
};
pub use service_def_loader::{
    ensure_service_definition_dir, service_definition_dir, service_definition_path,
    validate_service_definition_for_service, ServiceDefinitionError, ServiceDefinitionLoader,
    SERVICE_DEF_DIR_ENV, SERVICE_DEF_EXTENSION,
};
pub use version_allow_list::{check_version_allowed, VersionPolicyBlock};
