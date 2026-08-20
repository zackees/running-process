//! Host-neutral PTY facade owned by `running-process-platform-internal`.

pub use running_process_platform_internal::platform::terminal::{
    PtyBackend, PtyChild, PtyMaster, PtySize, PtySlave,
};

pub(crate) use running_process_platform_internal::platform::terminal::Backend;
