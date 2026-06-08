//! Broker server foundation for `running-process-broker-v1`.
//!
//! Phase 4 (#235) grows this module into the pipe accept loop, service
//! definition loader, backend registry, admin verbs, and perf guard.
//! The first slice keeps the core Hello validation and negotiation
//! logic testable without binding sockets or spawning backends.

pub mod hello_handler;

pub use hello_handler::{
    HelloHandler, HelloHandlerError, HelloRequest, PeerIdentity, RegisteredBackend,
};
