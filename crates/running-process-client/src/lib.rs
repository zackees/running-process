pub mod client;
pub mod paths;

pub use client::{
    connect_or_start, daemonize_command, launch_detached, ClientError, DaemonClient,
    SpawnCommandRequest, SpawnedDaemon,
};
