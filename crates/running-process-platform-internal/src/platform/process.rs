//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

pub use crate::platform_imp::exit_code;

#[derive(Clone, Copy)]
pub enum ObserverScope { SystemWide, LaunchedProcessTree }
#[derive(Clone, Copy)]
pub enum ObserverCategory { File, Network, Process }
#[derive(Clone, Copy)]
pub enum ObserverSupport { Supported, Partial, Unavailable }
#[derive(Clone, Copy)]
pub struct ObserverBackend { pub support: ObserverSupport, pub backend: &'static str, pub reason: &'static str }
pub use crate::platform_imp::observer_backend;
pub use crate::platform_imp::read_process_file_handles;

/// Platform-neutral Unix signal selectors used by the compatibility facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignalKind { Interrupt, Terminate, Kill }

pub use crate::platform_imp::{unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw};

pub use crate::platform_imp::kill_tree;
