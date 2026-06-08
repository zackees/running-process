//! Prost-generated types from `proto/broker_v1_*.proto`.
//!
//! All three .proto files share the `running_process.broker.v1`
//! package, so prost-build emits a single Rust module containing
//! every message and enum (Frame, Hello, HelloReply, Refused,
//! Negotiated, CacheManifest, ServiceDefinition, LifecycleEvent, ...).

include!(concat!(
    env!("OUT_DIR"),
    "/running_process.broker.v1.rs"
));
