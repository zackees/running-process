//! Generated protobuf types used by the optional running-process broker client.
//!
//! This is an implementation-detail package.  Consumers should use the
//! client-gated compatibility paths re-exported by `running-process` rather
//! than depending on this crate directly.

/// Generated daemon control protocol types.
#[allow(missing_docs)]
pub mod daemon {
    include!(concat!(env!("OUT_DIR"), "/running_process.daemon.v1.rs"));
}

/// Generated broker protocol types, grouped by frozen wire version.
pub mod broker {
    /// Generated v1 broker protocol types.
    #[allow(missing_docs)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/running_process.broker.v1.rs"));
    }

    /// Generated v2 broker protocol types.
    #[allow(missing_docs)]
    pub mod v2 {
        include!(concat!(env!("OUT_DIR"), "/running_process.broker.v2.rs"));
    }
}
