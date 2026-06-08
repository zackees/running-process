//! Broker server foundation for `running-process-broker-v1`.
//!
//! Phase 4 (#235) grows this module into the pipe accept loop, service
//! definition loader, backend registry, admin verbs, and perf guard.
//! The first slice keeps the core Hello validation and negotiation
//! logic testable without binding sockets or spawning backends.

pub mod admin;
pub mod backend_endpoint_allocator;
pub mod backend_registry;
pub mod connection;
pub mod hello_handler;
pub mod hello_router;
pub mod instance;
pub mod metrics;
pub mod perf_guard;
pub mod serve;
pub mod service_def_loader;
pub mod spawn_coordinator;
pub mod trace_context;
pub mod version_allow_list;

pub use admin::{AdminBackend, AdminSnapshot, AdminSpawnBudget, ADMIN_SCHEMA_VERSION};
pub use backend_endpoint_allocator::{
    BackendEndpointAllocator, BackendEndpointAllocatorError, DEFAULT_BACKEND_ENDPOINT_ATTEMPTS,
};
pub use backend_registry::{BackendKey, BackendRegistry};
pub use connection::{
    handle_hello_connection, handle_hello_connection_with, local_socket_name,
    serve_local_socket_connections, serve_one_local_socket, BrokerConnectionError,
    HelloResponder,
};
pub use hello_handler::{
    HelloHandler, HelloHandlerError, HelloRequest, PeerIdentity, RegisteredBackend,
};
pub use hello_router::HelloRouter;
pub use instance::{BrokerInstanceError, BrokerInstanceKey};
pub use perf_guard::{
    enforce_hello_latency_budget, summarize_hello_latencies, HelloLatencySummary, PerfGuardError,
    HELLO_P50_BUDGET, HELLO_P99_BUDGET, HELLO_PERF_SAMPLE_COUNT,
};
pub use serve::{
    build_hello_handler, serve_registered_backend, BrokerServeConfig, BrokerServeError,
};
pub use service_def_loader::{
    ensure_service_definition_dir, service_definition_dir, service_definition_path,
    validate_service_definition_for_service, ServiceDefinitionError, ServiceDefinitionLoader,
    SERVICE_DEF_DIR_ENV, SERVICE_DEF_EXTENSION,
};
pub use spawn_coordinator::{
    SpawnBeginError, SpawnBudgetConfig, SpawnBudgetSnapshot, SpawnCoordinator, SpawnOutcome,
    SpawnPermit, DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW, DEFAULT_SPAWN_BUDGET_WINDOW,
};
pub use trace_context::TraceContext;
pub use version_allow_list::{check_version_allowed, VersionPolicyBlock};
