//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

pub use crate::platform_imp::exit_code;

/// Platform-neutral Unix signal selectors used by the compatibility facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignalKind { Interrupt, Terminate, Kill }

pub use crate::platform_imp::{unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw};

pub use crate::platform_imp::kill_tree;
