//! Endpoint and process identity checks for backend handles.

use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;

use crate::broker::backend_lifecycle::identity::{DaemonProcess, IdentityError};
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle, VerifyPidError};
use crate::broker::protocol::{
    self, read_frame, write_frame, Endpoint, Frame, FrameKind, FramingError, PayloadEncoding,
    ENVELOPE_VERSION, MAX_FRAME_BYTES,
};

const PROTOCOL_VERSION: u32 = 1;
const PROBE_NONCE_BYTES: usize = 32;
const NONBLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Payload protocol reserved for `BackendHandle` endpoint identity probes.
pub const BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL: u32 = 0xB232;

/// Default deadline for the active endpoint-response proof.
pub const DEFAULT_ENDPOINT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Verify that an endpoint refers to the expected daemon process.
pub fn probe_endpoint(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<ProcessHandle, ProbeError> {
    if !same_endpoint(endpoint, &expected.ipc_endpoint) {
        return Err(ProbeError::EndpointMismatch);
    }
    let process_handle =
        verify_pid::verify_daemon_process(expected).map_err(ProbeError::VerifyPid)?;
    probe_endpoint_response(endpoint, expected)?;
    Ok(process_handle)
}

/// Compare two endpoint identities exactly.
pub fn same_endpoint(left: &Endpoint, right: &Endpoint) -> bool {
    left.namespace_id == right.namespace_id && left.path == right.path
}

/// Actively probe a backend endpoint and verify that it returns the expected
/// daemon identity.
///
/// The probe uses the broker v1 frame layout with a dedicated payload protocol.
/// Requests carry a 32-byte nonce. Responses must echo that nonce and include a
/// prost-encoded `DaemonProcess` payload that exactly matches `expected`.
pub fn probe_endpoint_response(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<(), EndpointProbeError> {
    probe_endpoint_response_with_timeout(endpoint, expected, DEFAULT_ENDPOINT_PROBE_TIMEOUT)
}

/// Timed variant of [`probe_endpoint_response`] used by tests and diagnostics.
pub fn probe_endpoint_response_with_timeout(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
    timeout: Duration,
) -> Result<(), EndpointProbeError> {
    let mut nonce = [0_u8; PROBE_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(EndpointProbeError::Random)?;
    let request_id = u64::from_le_bytes(nonce[..8].try_into().expect("nonce has 8 bytes"));
    let request_frame = endpoint_probe_request_frame(request_id, &nonce);
    let mut request_bytes = Vec::new();
    request_frame
        .encode(&mut request_bytes)
        .map_err(EndpointProbeError::EncodeFrame)?;

    let deadline = Instant::now() + timeout;
    let mut stream = connect_endpoint(endpoint)?;
    stream
        .set_nonblocking(true)
        .map_err(EndpointProbeError::ConfigureNonblocking)?;
    write_probe_frame_with_deadline(&mut stream, &request_bytes, deadline)?;

    let response_bytes = read_probe_frame_with_deadline(&mut stream, deadline)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(EndpointProbeError::DecodeFrame)?;
    validate_endpoint_probe_response_frame(&response_frame, request_id)?;
    let actual = decode_response_identity(&response_frame.payload, &nonce)?;
    if !same_daemon_identity(&actual, expected) {
        return Err(identity_mismatch(expected, &actual));
    }
    Ok(())
}

/// Decoded endpoint probe request for backend-side responders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointProbeRequest {
    /// Request frame ID that the response must echo.
    pub request_id: u64,
    /// Random challenge that the response must echo.
    pub nonce: [u8; PROBE_NONCE_BYTES],
    /// Trace context copied from the request frame, if any.
    pub traceparent: String,
    /// Trace state copied from the request frame, if any.
    pub tracestate: String,
}

/// Read and validate one endpoint probe request from an accepted IPC stream.
pub fn read_endpoint_probe_request<S: Read>(
    stream: &mut S,
) -> Result<EndpointProbeRequest, EndpointProbeServerError> {
    let request_bytes = read_frame(stream)?;
    let frame =
        Frame::decode(request_bytes.as_slice()).map_err(EndpointProbeServerError::DecodeFrame)?;
    validate_endpoint_probe_request_frame(&frame)?;
    let nonce = frame
        .payload
        .as_slice()
        .try_into()
        .map_err(|_| EndpointProbeServerError::MalformedPayload("nonce must be 32 bytes"))?;
    Ok(EndpointProbeRequest {
        request_id: frame.request_id,
        nonce,
        traceparent: frame.traceparent,
        tracestate: frame.tracestate,
    })
}

/// Write one endpoint probe response for a validated request.
pub fn write_endpoint_probe_response<S: Write>(
    stream: &mut S,
    request: &EndpointProbeRequest,
    daemon: &DaemonProcess,
) -> Result<(), EndpointProbeServerError> {
    let response_frame = endpoint_probe_response_frame(request, daemon);
    let mut response_bytes = Vec::new();
    response_frame
        .encode(&mut response_bytes)
        .map_err(EndpointProbeServerError::EncodeFrame)?;
    write_frame(stream, &response_bytes)?;
    Ok(())
}

/// Serve exactly one endpoint probe request on an already-accepted IPC stream.
pub fn handle_endpoint_probe<S: Read + Write>(
    stream: &mut S,
    daemon: &DaemonProcess,
) -> Result<(), EndpointProbeServerError> {
    let request = read_endpoint_probe_request(stream)?;
    write_endpoint_probe_response(stream, &request, daemon)
}

/// Errors returned while probing a backend endpoint.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The caller-provided endpoint did not match the expected daemon endpoint.
    #[error("endpoint does not match expected daemon identity")]
    EndpointMismatch,
    /// The endpoint did not answer the active identity probe as expected.
    #[error(transparent)]
    EndpointResponse(#[from] EndpointProbeError),
    /// The daemon process identity could not be verified.
    #[error(transparent)]
    VerifyPid(#[from] VerifyPidError),
}

/// Errors returned by the active endpoint-response probe.
#[derive(Debug, thiserror::Error)]
pub enum EndpointProbeError {
    /// The probe nonce could not be generated.
    #[error("backend endpoint probe random generation failed: {0}")]
    Random(getrandom::Error),
    /// The endpoint path/name could not be converted to a local socket name.
    #[error("backend endpoint probe local-socket name failed: {0}")]
    LocalSocketName(io::Error),
    /// Connecting to the endpoint failed.
    #[error("backend endpoint probe connect failed: {0}")]
    Connect(io::Error),
    /// The stream could not be switched to nonblocking mode for deadline I/O.
    #[error("backend endpoint probe nonblocking setup failed: {0}")]
    ConfigureNonblocking(io::Error),
    /// Probe I/O exceeded the configured deadline.
    #[error("backend endpoint probe timed out")]
    Timeout,
    /// Raw probe I/O failed.
    #[error("backend endpoint probe I/O failed: {0}")]
    Io(io::Error),
    /// The peer used the wrong broker framing byte.
    #[error("backend endpoint probe unsupported framing version: got {got}, expected {expected}")]
    UnsupportedFramingVersion {
        /// Framing byte received from the peer.
        got: u8,
        /// Framing byte expected by v1.
        expected: u8,
    },
    /// The peer advertised a frame that exceeds the v1 frame cap.
    #[error("backend endpoint probe frame body too large: {body_length} bytes exceeds cap {cap}")]
    FrameTooLarge {
        /// Advertised frame body length.
        body_length: usize,
        /// Maximum accepted frame body length.
        cap: usize,
    },
    /// The outbound probe request frame could not be encoded.
    #[error("failed to encode endpoint probe frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The response frame could not be decoded.
    #[error("failed to decode endpoint probe response Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The response frame did not match the endpoint-probe contract.
    #[error("unexpected endpoint probe response: {0}")]
    UnexpectedFrame(&'static str),
    /// The response payload did not match the endpoint-probe contract.
    #[error("endpoint probe response payload is malformed: {0}")]
    MalformedPayload(&'static str),
    /// The response daemon identity could not be decoded.
    #[error("failed to decode endpoint probe daemon identity: {0}")]
    DecodeDaemonProcess(prost::DecodeError),
    /// The response daemon identity was malformed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The response daemon identity did not match the expected identity.
    #[error("endpoint probe response identity did not match expected daemon identity: {field}")]
    IdentityMismatch {
        /// First mismatched identity field.
        field: &'static str,
    },
}

/// Errors returned by backend-side endpoint probe responders.
#[derive(Debug, thiserror::Error)]
pub enum EndpointProbeServerError {
    /// v1 framing failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The request frame could not be decoded.
    #[error("failed to decode endpoint probe request Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The response frame could not be encoded.
    #[error("failed to encode endpoint probe response Frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The request frame did not match the endpoint-probe contract.
    #[error("unexpected endpoint probe request: {0}")]
    UnexpectedFrame(&'static str),
    /// The request payload did not match the endpoint-probe contract.
    #[error("endpoint probe request payload is malformed: {0}")]
    MalformedPayload(&'static str),
}

fn endpoint_probe_request_frame(request_id: u64, nonce: &[u8; PROBE_NONCE_BYTES]) -> Frame {
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload: nonce.to_vec(),
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn endpoint_probe_response_frame(request: &EndpointProbeRequest, daemon: &DaemonProcess) -> Frame {
    let mut payload = Vec::with_capacity(PROBE_NONCE_BYTES + 128);
    payload.extend_from_slice(&request.nonce);
    daemon.to_proto().encode(&mut payload).expect(
        "prost encoding DaemonProcess into Vec cannot fail because Vec writes are infallible",
    );

    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Response as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload,
        request_id: request.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request.traceparent.clone(),
        tracestate: request.tracestate.clone(),
    }
}

fn validate_endpoint_probe_request_frame(frame: &Frame) -> Result<(), EndpointProbeServerError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "envelope_version is not v1",
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "kind is not REQUEST",
        ));
    }
    if frame.payload_protocol != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "payload_protocol is not endpoint probe",
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "payload is compressed",
        ));
    }
    if frame.payload.len() != PROBE_NONCE_BYTES {
        return Err(EndpointProbeServerError::MalformedPayload(
            "nonce must be 32 bytes",
        ));
    }
    Ok(())
}

fn validate_endpoint_probe_response_frame(
    frame: &Frame,
    request_id: u64,
) -> Result<(), EndpointProbeError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(EndpointProbeError::UnexpectedFrame(
            "envelope_version is not v1",
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Response) {
        return Err(EndpointProbeError::UnexpectedFrame("kind is not RESPONSE"));
    }
    if frame.payload_protocol != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
        return Err(EndpointProbeError::UnexpectedFrame(
            "payload_protocol is not endpoint probe",
        ));
    }
    if frame.request_id != request_id {
        return Err(EndpointProbeError::UnexpectedFrame(
            "request_id does not match endpoint probe request",
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(EndpointProbeError::UnexpectedFrame("payload is compressed"));
    }
    Ok(())
}

fn decode_response_identity(
    payload: &[u8],
    expected_nonce: &[u8; PROBE_NONCE_BYTES],
) -> Result<DaemonProcess, EndpointProbeError> {
    if payload.len() < PROBE_NONCE_BYTES {
        return Err(EndpointProbeError::MalformedPayload(
            "payload shorter than nonce",
        ));
    }
    let (nonce, identity_bytes) = payload.split_at(PROBE_NONCE_BYTES);
    if nonce != expected_nonce {
        return Err(EndpointProbeError::UnexpectedFrame(
            "nonce does not match endpoint probe request",
        ));
    }
    let proto_identity = protocol::DaemonProcess::decode(identity_bytes)
        .map_err(EndpointProbeError::DecodeDaemonProcess)?;
    DaemonProcess::try_from(proto_identity).map_err(EndpointProbeError::Identity)
}

fn identity_mismatch(expected: &DaemonProcess, actual: &DaemonProcess) -> EndpointProbeError {
    let field = if actual.pid != expected.pid {
        "pid"
    } else if actual.exe_path != expected.exe_path {
        "exe_path"
    } else if actual.exe_sha256 != expected.exe_sha256 {
        "exe_sha256"
    } else if actual.boot_id != expected.boot_id {
        "boot_id"
    } else if !same_endpoint(&actual.ipc_endpoint, &expected.ipc_endpoint) {
        "ipc_endpoint"
    } else {
        "unknown"
    };
    EndpointProbeError::IdentityMismatch { field }
}

fn same_daemon_identity(left: &DaemonProcess, right: &DaemonProcess) -> bool {
    left.pid == right.pid
        && left.exe_path == right.exe_path
        && left.exe_sha256 == right.exe_sha256
        && left.boot_id == right.boot_id
        && same_endpoint(&left.ipc_endpoint, &right.ipc_endpoint)
}

fn connect_endpoint(
    endpoint: &Endpoint,
) -> Result<interprocess::local_socket::Stream, EndpointProbeError> {
    if endpoint.path.is_empty() {
        return Err(EndpointProbeError::Connect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backend endpoint path is empty",
        )));
    }
    let name = endpoint_name(&endpoint.path).map_err(EndpointProbeError::LocalSocketName)?;
    interprocess::local_socket::Stream::connect(name).map_err(EndpointProbeError::Connect)
}

fn write_probe_frame_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    body: &[u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(EndpointProbeError::FrameTooLarge {
            body_length: body.len(),
            cap: MAX_FRAME_BYTES,
        });
    }
    let mut wire = Vec::with_capacity(1 + 4 + body.len());
    wire.push(ENVELOPE_VERSION);
    wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wire.extend_from_slice(body);
    write_all_with_deadline(stream, &wire, deadline)?;
    flush_with_deadline(stream, deadline)
}

fn read_probe_frame_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    deadline: Instant,
) -> Result<Vec<u8>, EndpointProbeError> {
    let mut version = [0_u8; 1];
    read_exact_with_deadline(stream, &mut version, deadline)?;
    if version[0] != ENVELOPE_VERSION {
        return Err(EndpointProbeError::UnsupportedFramingVersion {
            got: version[0],
            expected: ENVELOPE_VERSION,
        });
    }

    let mut len = [0_u8; 4];
    read_exact_with_deadline(stream, &mut len, deadline)?;
    let body_length = u32::from_le_bytes(len) as usize;
    if body_length > MAX_FRAME_BYTES {
        return Err(EndpointProbeError::FrameTooLarge {
            body_length,
            cap: MAX_FRAME_BYTES,
        });
    }

    let mut body = vec![0_u8; body_length];
    if body_length > 0 {
        read_exact_with_deadline(stream, &mut body, deadline)?;
    }
    Ok(body)
}

fn write_all_with_deadline<W: Write>(
    writer: &mut W,
    mut buf: &[u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => {
                return Err(EndpointProbeError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "endpoint probe write returned zero bytes",
                )));
            }
            Ok(written) => buf = &buf[written..],
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
    Ok(())
}

fn read_exact_with_deadline<R: Read>(
    reader: &mut R,
    mut buf: &mut [u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => wait_for_io(deadline)?,
            Ok(read) => {
                let tmp = buf;
                buf = &mut tmp[read..];
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
    Ok(())
}

fn flush_with_deadline<W: Write>(
    writer: &mut W,
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
}

fn wait_for_io(deadline: Instant) -> Result<(), EndpointProbeError> {
    if Instant::now() >= deadline {
        return Err(EndpointProbeError::Timeout);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(remaining.min(NONBLOCKING_POLL_INTERVAL));
    Ok(())
}

fn endpoint_name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.to_ns_name::<GenericNamespaced>()
    }
}
