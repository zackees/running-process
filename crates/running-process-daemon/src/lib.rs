pub mod client;
pub mod config;
pub mod handlers;
pub mod idle;
pub mod paths;
pub mod platform;
pub mod server;
pub mod shadow;

// Re-export the server socket_path helper for convenience in tests.
pub use server::socket_path as server_socket_path;
