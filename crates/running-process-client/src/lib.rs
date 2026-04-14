pub mod client;
pub mod paths;

pub use client::{
    connect_or_start, daemonize_command, ClientError, DaemonClient, SpawnCommandRequest,
    SpawnedDaemon,
};
