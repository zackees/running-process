//! Neutral indexes for host-mechanics capabilities.
//!
//! Concrete host selection and implementations are private to this package.
//! Callers will use these stable capability names rather than selecting a host.

pub mod autostart;
pub mod executable;
pub mod fs;
pub mod host;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod private_dir;
pub mod process;
pub mod resources;
#[cfg(feature = "pty")]
pub mod terminal;
#[cfg(feature = "terminal-graphics")]
pub mod terminal_graphics;
pub mod terminal_input;
pub mod window_icon;
