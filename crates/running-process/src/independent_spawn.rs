//! Opt-in native independent-launch substrate.

pub use running_process_platform_internal::platform::independent_spawn::{
    run_launcher, spawn, IndependentChild, LaunchSpec, Readiness,
};
