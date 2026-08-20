//! Local endpoint, listener, connection, peer, handoff, and security primitives.
//!
//! Endpoint strings and protocol policy remain with callers. These opaque
//! values own the selected host transport so callers never name Unix sockets,
//! Windows named pipes, `interprocess` types, file descriptors, or handles.

#[cfg(feature = "ipc")]
pub use crate::{
    ipc_current_user_id as current_user_id, IpcEndpoint as Endpoint, IpcListener as Listener,
    IpcListenerNonblockingMode as ListenerNonblockingMode, IpcPeerIdentity as PeerIdentity,
    IpcStream as Stream,
};

#[cfg(feature = "ipc-async")]
pub use crate::{
    IpcAsyncListener as AsyncListener, IpcAsyncStream as AsyncStream,
    IpcIntoAsyncListener as IntoAsyncListener, IpcIntoAsyncStream as IntoAsyncStream,
};

#[cfg(all(test, feature = "ipc"))]
mod tests {
    use std::io::{Read, Write};

    use super::{current_user_id, Endpoint, Listener, Stream};

    #[test]
    fn endpoint_lifecycle_mechanics_are_facade_owned() {
        let endpoint = Endpoint::test("lifecycle").expect("test endpoint");
        endpoint.retire().expect("retire absent endpoint");

        let listener = Listener::bind(&endpoint).expect("bind endpoint");

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[test]
    fn sync_bind_accept_connect_and_peer_identity_round_trip() {
        let endpoint = Endpoint::test("sync-roundtrip").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write response");
        });

        let mut client = Stream::connect(&endpoint).expect("connect");
        client.write_all(b"ping").expect("write request");
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).expect("read response");
        assert_eq!(&response, b"pong");
        server.join().expect("server thread");
    }

    #[cfg(feature = "ipc-async")]
    #[tokio::test]
    async fn async_bind_accept_connect_and_peer_identity_round_trip() {
        use super::{AsyncListener, AsyncStream};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let endpoint = Endpoint::test("async-roundtrip").expect("test endpoint");
        let listener = AsyncListener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.expect("write response");
        });

        let mut client = AsyncStream::connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write request");
        let mut response = [0_u8; 4];
        client
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"pong");
        server.await.expect("server task");
    }
}
