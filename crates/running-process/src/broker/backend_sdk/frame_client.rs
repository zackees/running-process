//! Blocking frame client with built-in request correlation (#412).
//!
//! Before #412, every consumer invented a per-connection request-id
//! counter and re-implemented send/receive plumbing for the v1 frame
//! wire. `FrameClient` owns both: it assigns monotonically increasing
//! request ids, frames the payload, and validates that the response
//! echoes the id and payload protocol.
//!
//! This client is **blocking**. The default `client` cargo feature
//! carries no async runtime, so async daemons either wrap calls in
//! their runtime's `spawn_blocking` or enable the `client-async`
//! feature and use the async twin
//! [`AsyncFrameClient`](super::AsyncFrameClient) instead (#414).
//! Calling [`FrameClient::request`] from a tokio task without
//! `spawn_blocking` will block the runtime worker thread.
//!
//! [`BackendHandle::probe_with_service`]: crate::broker::backend_handle::BackendHandle::probe_with_service

use std::io;

use prost::Message;

use crate::broker::protocol::{read_frame, write_frame, Endpoint, Frame, FrameKind, FramingError};

/// Blocking request/response client for a backend daemon's frame lane.
///
/// ```no_run
/// use running_process::broker::backend_sdk::FrameClient;
/// use running_process::broker::protocol::Endpoint;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let endpoint = Endpoint::unix_socket("my-daemon", "/tmp/my-daemon.sock")?;
/// let mut client = FrameClient::connect(&endpoint)?;
/// let response = client.request(0x7A63, b"ping".to_vec())?;
/// assert_eq!(response.payload, b"pong");
/// # Ok(())
/// # }
/// ```
pub struct FrameClient {
    stream: io::BufReader<interprocess::local_socket::Stream>,
    next_request_id: u64,
}

impl FrameClient {
    /// Connect to a backend endpoint using the platform local-socket
    /// name type (bare pipe name on Windows, filesystem path on Unix).
    pub fn connect(endpoint: &Endpoint) -> Result<Self, FrameClientError> {
        let connection = crate::broker::backend_handle::Connection::connect(endpoint)
            .map_err(FrameClientError::Connect)?;
        Ok(Self::from_stream(connection.into_inner()))
    }

    /// Wrap an already-connected local-socket stream (e.g. one opened
    /// through a verified
    /// [`BackendHandle`](crate::broker::backend_handle::BackendHandle)).
    pub fn from_stream(stream: interprocess::local_socket::Stream) -> Self {
        Self {
            stream: io::BufReader::new(stream),
            next_request_id: 1,
        }
    }

    /// Send one request frame and block until its response arrives.
    ///
    /// Assigns the next request id, sends
    /// `Frame::request(payload_protocol, payload)`, then reads frames
    /// until one echoes the id. The returned frame is validated to be a
    /// `RESPONSE` on the same payload protocol.
    pub fn request(
        &mut self,
        payload_protocol: u32,
        payload: Vec<u8>,
    ) -> Result<Frame, FrameClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);

        let frame = Frame::request(payload_protocol, payload).with_request_id(request_id);
        let mut body = Vec::with_capacity(frame.encoded_len());
        frame
            .encode(&mut body)
            .expect("prost encoding into Vec cannot fail because Vec writes are infallible");
        write_frame(self.stream.get_mut(), &body)?;

        let response_bytes = read_frame(&mut self.stream)?;
        let response =
            Frame::decode(response_bytes.as_slice()).map_err(FrameClientError::Decode)?;
        if response.request_id != request_id {
            return Err(FrameClientError::RequestIdMismatch {
                expected: request_id,
                got: response.request_id,
            });
        }
        if response.payload_protocol != payload_protocol {
            return Err(FrameClientError::PayloadProtocolMismatch {
                expected: payload_protocol,
                got: response.payload_protocol,
            });
        }
        if FrameKind::try_from(response.kind) != Ok(FrameKind::Response) {
            return Err(FrameClientError::NotAResponse {
                kind: response.kind,
            });
        }
        Ok(response)
    }

    /// The request id the next [`Self::request`] call will use.
    pub fn next_request_id(&self) -> u64 {
        self.next_request_id
    }
}

/// Errors returned by [`FrameClient`].
#[derive(Debug, thiserror::Error)]
pub enum FrameClientError {
    /// Opening the IPC connection failed.
    #[error("frame client connect failed: {0}")]
    Connect(io::Error),
    /// v1 framing failed on the wire.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The response body was not a valid prost `Frame`.
    #[error("failed to decode response Frame: {0}")]
    Decode(prost::DecodeError),
    /// The response did not echo the request id.
    #[error("response request_id {got} does not match request {expected}")]
    RequestIdMismatch {
        /// The id this client assigned to the request.
        expected: u64,
        /// The id the peer echoed.
        got: u64,
    },
    /// The response switched payload protocols mid-correlation.
    #[error("response payload_protocol {got:#06X} does not match request {expected:#06X}")]
    PayloadProtocolMismatch {
        /// The payload protocol the request used.
        expected: u32,
        /// The payload protocol the peer answered with.
        got: u32,
    },
    /// The correlated frame was not a `RESPONSE`.
    #[error("correlated frame kind {kind} is not RESPONSE")]
    NotAResponse {
        /// The raw frame kind received.
        kind: i32,
    },
}
