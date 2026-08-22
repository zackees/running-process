//! Neutral indexes for host-mechanics capabilities.
//!
//! Concrete host selection and implementations are private to this package.
//! Callers will use these stable capability names rather than selecting a host.

pub mod autostart;
pub mod executable;
pub mod fs;
pub mod host;
pub mod ipc;
pub mod process;
pub mod resources;
pub mod terminal;
pub mod terminal_input;
pub mod window_icon;
