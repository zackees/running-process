//! Shared backend lifecycle primitives used by broker and direct clients.

pub mod identity;
pub mod probe;
pub mod verify_pid;

pub use identity::DaemonProcess;
