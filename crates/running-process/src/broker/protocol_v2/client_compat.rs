//! v1-client compatibility surface under the v2 namespace
//! (slice 25-A of zccache#782).
//!
//! Re-exports the v1 broker-client + adopt types from [`super::super::client`]
//! and [`super::super::adopt`] under the v2 namespace so downstream consumers
//! can complete the literal "no `running_process::broker::{adopt,client}::*`
//! imports" milestone of their v1→v2 burn-down without breaking the
//! production broker connection path.
//!
//! ## Why a re-export, not a parallel client
//!
//! The "true" v2 broker client ([`super::super::client_v2`]) is already
//! published — what it lacks is a v2 broker SERVER to connect to. The
//! `running-process-broker-v2` binary is a scaffold (PRs #486–#489) without
//! an accept loop yet; consumers that need to actually adopt + handle
//! traffic still have to dial the v1 broker.
//!
//! Forcing consumers to keep `use running_process::broker::client::*`
//! imports during this window pollutes their dependency graph with a
//! "v1 surface" marker that lives forever in PR diffs and grep output.
//! The re-export under `protocol_v2::client_compat` is the honest
//! intermediate state: "consumer depends on the v2 namespace for its
//! broker types; the implementation under the namespace is v1 until
//! v2 broker is feature-complete."
//!
//! When [`super::super::client_v2::connect`] becomes production-ready
//! (accepting hello frames from a real v2 broker, threading adopt
//! through `BrokeredBackend`), this module's re-exports get swapped
//! for `client_v2::*` equivalents. The CONSUMER side doesn't change.
//!
//! ## Migration contract
//!
//! Replace:
//! ```rust,ignore
//! use running_process::broker::adopt::{AdoptError, AsyncBrokerSession, OwnedConnectRequest};
//! use running_process::broker::client::{BrokerClientError, BackendConnectionRoute, RefusalKind};
//! ```
//! with:
//! ```rust,ignore
//! use running_process::broker::protocol_v2::client_compat::{
//!     AdoptError, AsyncBrokerSession, OwnedConnectRequest,
//!     BrokerClientError, BackendConnectionRoute, RefusalKind,
//! };
//! ```
//!
//! Identical Rust API, identical wire behaviour, identical errors.

// Re-export every v1 adopt symbol zccache consumes. `AsyncBrokerSession`
// + `OwnedConnectRequest` are gated on `client-async` (#433 R3) — both
// upstream and downstream zccache enable that feature, but mirror the
// gate here so the re-export compiles with `--features client` alone.
pub use super::super::adopt::AdoptError;

#[cfg(feature = "client-async")]
pub use super::super::adopt::{
    AsyncBrokerSession, IntoBackendIoError, OwnedBackendIo, OwnedConnectRequest,
};

// Re-export every v1 client symbol zccache consumes.
pub use super::super::client::{BackendConnectionRoute, BrokerClientError, RefusalKind};

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice 25-A contract: every v1 broker-client + adopt symbol zccache
    /// imports is reachable through the v2 namespace at the same TypeId.
    /// A future upstream rename or fork catches here as a build break.
    #[test]
    fn v1_client_adopt_types_are_aliased_under_v2_namespace() {
        use std::any::TypeId;

        // adopt: AdoptError always; AsyncBrokerSession + OwnedConnectRequest
        // gated on `client-async` (#433 R3).
        assert_eq!(
            TypeId::of::<super::super::super::adopt::AdoptError>(),
            TypeId::of::<AdoptError>(),
            "AdoptError aliased"
        );
        #[cfg(feature = "client-async")]
        {
            assert_eq!(
                TypeId::of::<super::super::super::adopt::AsyncBrokerSession>(),
                TypeId::of::<AsyncBrokerSession>(),
                "AsyncBrokerSession aliased"
            );
            assert_eq!(
                TypeId::of::<super::super::super::adopt::OwnedConnectRequest>(),
                TypeId::of::<OwnedConnectRequest>(),
                "OwnedConnectRequest aliased"
            );
        }

        // client: BackendConnectionRoute, BrokerClientError, RefusalKind.
        assert_eq!(
            TypeId::of::<super::super::super::client::BackendConnectionRoute>(),
            TypeId::of::<BackendConnectionRoute>(),
            "BackendConnectionRoute aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::BrokerClientError>(),
            TypeId::of::<BrokerClientError>(),
            "BrokerClientError aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::RefusalKind>(),
            TypeId::of::<RefusalKind>(),
            "RefusalKind aliased"
        );
    }
}
