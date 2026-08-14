//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

pub use crate::platform_imp::exit_code;

pub use crate::platform_imp::kill_tree;
