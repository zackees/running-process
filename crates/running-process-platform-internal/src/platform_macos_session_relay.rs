//! Portable local-socket relay for macOS.

/// Relay a pair of established local sockets through Tokio's bounded buffers.
pub async fn relay_local_socket_session(
    mut client: interprocess::local_socket::tokio::Stream,
    mut daemon: interprocess::local_socket::tokio::Stream,
) -> std::io::Result<()> {
    tokio::io::copy_bidirectional(&mut client, &mut daemon).await?;
    Ok(())
}
