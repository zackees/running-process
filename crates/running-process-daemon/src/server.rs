use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::ListenerOptions;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::tokio::prelude::*;
use prost::Message;
use tokio::sync::watch;
use tokio::time::{timeout, Duration};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, error, info, warn};

use running_process_proto::daemon::{
    DaemonRequest, DaemonResponse, RequestType, StatusCode,
};

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Returns the platform-appropriate IPC socket path.
///
/// - **Unix**: `$XDG_RUNTIME_DIR/running-process/daemon{-hash}.sock`
///   (fallback: `/tmp/running-process-{uid}/daemon{-hash}.sock`)
/// - **Windows**: `\\.\pipe\running-process-daemon-{username}{-hash}`
///
/// If `scope_hash` is `Some(h)`, appends `-{h}` to the base name.
pub fn socket_path(scope_hash: Option<&str>) -> String {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };

    #[cfg(unix)]
    {
        let dir = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            std::path::PathBuf::from(runtime_dir).join("running-process")
        } else {
            let uid = unsafe { libc::getuid() };
            std::path::PathBuf::from(format!("/tmp/running-process-{uid}"))
        };
        format!(
            "{}/daemon{suffix}.sock",
            dir.display()
        )
    }

    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        format!(r"\\.\pipe\running-process-daemon-{username}{suffix}")
    }
}

// ---------------------------------------------------------------------------
// DaemonServer
// ---------------------------------------------------------------------------

pub struct DaemonServer {
    socket_path: String,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl DaemonServer {
    pub fn new(socket_path: String) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            socket_path,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Signal all accept loops and connection handlers to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Run the IPC server, blocking until shutdown is signalled.
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Platform-specific: create parent directory for Unix socket files.
        #[cfg(unix)]
        {
            if let Some(parent) = std::path::Path::new(&self.socket_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Remove stale socket file if present.
            let _ = std::fs::remove_file(&self.socket_path);
        }

        let name = self.create_socket_name()?;

        let listener = ListenerOptions::new()
            .name(name)
            .create_tokio()?;

        // On Unix, set socket file permissions to owner-only (0o600).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&self.socket_path, perms)?;
        }

        info!("daemon listening on {}", self.socket_path);

        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok(stream) => {
                            let peer_shutdown = self.shutdown_rx.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer_shutdown).await {
                                    warn!("connection handler error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                            // Brief pause to avoid tight error loops.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("shutdown signal received, stopping listener");
                        break;
                    }
                }
            }
        }

        // Cleanup socket file on Unix.
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.socket_path);
        }

        Ok(())
    }

    /// Convert the socket path string into an `interprocess::local_socket::Name`.
    ///
    /// On Windows, named pipes live in a namespace, so we use `ToNsName` with
    /// `GenericNamespaced`.  On Unix, sockets are filesystem paths, so we use
    /// `ToFsName` with `GenericFilePath`.
    fn create_socket_name(&self) -> Result<interprocess::local_socket::Name<'_>, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::ToFsName;
            Ok(self.socket_path.as_str().to_fs_name::<GenericFilePath>()?)
        }

        #[cfg(windows)]
        {
            use interprocess::local_socket::ToNsName;
            Ok(self.socket_path.as_str().to_ns_name::<GenericNamespaced>()?)
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Idle timeout for waiting on the next frame from a client.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum frame size (4 MiB).
const MAX_FRAME_LENGTH: usize = 4 * 1024 * 1024;

async fn handle_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME_LENGTH)
        .new_codec();

    let mut framed = Framed::new(stream, codec);

    loop {
        // Check for shutdown before blocking on read.
        if *shutdown_rx.borrow() {
            debug!("connection closing due to shutdown");
            break;
        }

        let frame: bytes::BytesMut = tokio::select! {
            result = timeout(IDLE_TIMEOUT, framed.next()) => {
                match result {
                    Ok(Some(Ok(bytes))) => bytes,
                    Ok(Some(Err(e))) => {
                        // Layer 1: frame decode error.
                        warn!("frame decode error: {e}");
                        let resp = error_response(0, StatusCode::InvalidArgument, format!("frame decode error: {e}"));
                        let _ = send_response(&mut framed, &resp).await;
                        break;
                    }
                    Ok(None) => {
                        // Client disconnected cleanly.
                        debug!("client disconnected");
                        break;
                    }
                    Err(_) => {
                        // Idle timeout.
                        debug!("connection idle timeout");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("connection closing due to shutdown");
                break;
            }
        };

        // Layer 2: protobuf decode.
        let request = match DaemonRequest::decode(frame.as_ref()) {
            Ok(req) => req,
            Err(e) => {
                warn!("protobuf decode error: {e}");
                let resp = error_response(0, StatusCode::InvalidArgument, format!("protobuf decode error: {e}"));
                let _ = send_response(&mut framed, &resp).await;
                continue;
            }
        };

        let request_id = request.id;

        // Layer 4: catch panics around the dispatch.
        let response = match catch_unwind(AssertUnwindSafe(|| dispatch_request(&request))) {
            Ok(future) => future.await,
            Err(_) => {
                error!("panic in request handler for request_id={request_id}");
                error_response(
                    request_id,
                    StatusCode::Internal,
                    "internal server error: handler panicked".into(),
                )
            }
        };

        if let Err(e) = send_response(&mut framed, &response).await {
            warn!("failed to send response for request_id={request_id}: {e}");
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Layer 3: dispatch based on `RequestType`.
///
/// All handlers currently return a stub "not implemented" response with
/// `StatusCode::Unavailable`. Real implementations come in later tasks.
fn dispatch_request(
    request: &DaemonRequest,
) -> impl Future<Output = DaemonResponse> + Send + 'static {
    let request_id = request.id;
    let request_type = request.r#type;

    // Try to decode the request type enum.
    let response = match RequestType::try_from(request_type) {
        Ok(RequestType::Unspecified) => {
            error_response(request_id, StatusCode::UnknownRequest, "unspecified request type".into())
        }
        Ok(rt) => {
            stub_response(request_id, rt)
        }
        Err(_) => {
            error_response(
                request_id,
                StatusCode::UnknownRequest,
                format!("unknown request type: {request_type}"),
            )
        }
    };

    // Return a ready future so the signature is uniform for when real
    // async handlers are added later.
    std::future::ready(response)
}

/// Build a stub "not implemented" response for a known request type.
fn stub_response(request_id: u64, rt: RequestType) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: StatusCode::Unavailable.into(),
        message: format!("{rt:?} not implemented yet"),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode and send a `DaemonResponse` over the framed transport.
async fn send_response<T>(
    framed: &mut Framed<T, LengthDelimitedCodec>,
    response: &DaemonResponse,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let encoded = response.encode_to_vec();
    framed.send(Bytes::from(encoded)).await?;
    Ok(())
}

/// Build an error `DaemonResponse` with no payload.
fn error_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code.into(),
        message,
        ..Default::default()
    }
}
