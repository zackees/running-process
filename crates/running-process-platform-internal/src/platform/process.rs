//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

pub use crate::platform_imp::{
    configure_trampoline_command, enable_descendant_subreaper, exit_code, set_process_name,
    trampoline_exit_code, process_snapshot, process_snapshot_for_pid, unix_mark_extra_fds_close_on_exec,
    configure_sync_contained_command, configure_sync_daemon_command,
    parent_has_console, sync_child_native_handle,
};

/// A platform-owned identity record used when observing a process tree.
/// The timestamp fields are opaque host-native creation-time components and
/// must only be compared for equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub parent_pid: u32,
    pub start_time_a: u64,
    pub start_time_b: u64,
}

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
pub use crate::platform_imp::read_process_cmdline;

/// Platform-neutral Unix signal selectors used by the compatibility facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignalKind { Interrupt, Terminate, Kill }

pub use crate::platform_imp::{unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw};

pub use crate::platform_imp::kill_tree;
