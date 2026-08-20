pub use crate::client::client;
pub use crate::client::paths;
pub use crate::client::pipe_session;
pub use crate::client::pty_session;

#[doc(hidden)]
pub use running_process_platform_internal::platform::ipc::IntoAsyncListener as IntoDaemonAsyncListener;

pub mod attach_stream;
pub mod backend_endpoint;
pub mod compile_session;
pub mod config;
pub mod emergency_reserve;
pub mod handlers;
pub mod idle;
pub mod observer_registry;
pub mod pipe_attach_stream;
pub mod pipe_sessions;
pub mod platform;
pub mod pty_sessions;
pub mod reaper;
pub mod registry;
pub mod runtime_gc;
pub mod server;
pub mod services;
pub mod services_snapshot;
pub mod session_endpoint;
pub mod shadow;
pub mod telemetry;
