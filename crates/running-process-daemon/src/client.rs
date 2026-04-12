//! Synchronous IPC client for the running-process daemon.
//!
//! Connects to the daemon over a local socket (Unix domain socket on
//! Linux/macOS, named pipe on Windows) and exchanges length-prefixed protobuf
//! messages.

use crate::paths;
use interprocess::local_socket::Stream;
use interprocess::TryClone;
use prost::Message;
use running_process_proto::daemon::{
    DaemonRequest, DaemonResponse, GetProcessTreeRequest, KillTreeRequest, KillZombiesRequest,
    ListActiveRequest, ListByOriginatorRequest, PingRequest, RequestType, ShutdownRequest,
    StatusRequest,
};
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by [`DaemonClient`] operations.
#[derive(Debug)]
pub enum ClientError {
    /// Failed to connect to the daemon socket.
    Connect(std::io::Error),
    /// I/O error during send or receive.
    Io(std::io::Error),
    /// Failed to decode a protobuf response.
    Decode(prost::DecodeError),
    /// The daemon is not running and could not be started.
    DaemonNotRunning,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect(e) => write!(f, "failed to connect to daemon: {e}"),
            ClientError::Io(e) => write!(f, "daemon I/O error: {e}"),
            ClientError::Decode(e) => write!(f, "failed to decode daemon response: {e}"),
            ClientError::DaemonNotRunning => write!(f, "daemon is not running"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Connect(e) | ClientError::Io(e) => Some(e),
            ClientError::Decode(e) => Some(e),
            ClientError::DaemonNotRunning => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Synchronous IPC client that communicates with the daemon over a local socket.
///
/// Messages are framed with a 4-byte big-endian length prefix followed by
/// a protobuf-encoded payload.
pub struct DaemonClient {
    reader: BufReader<Stream>,
    writer: BufWriter<Stream>,
    next_id: AtomicU64,
}

impl DaemonClient {
    /// Connect to a running daemon identified by an optional scope hash.
    ///
    /// The socket path is computed by [`paths::socket_path`] and the name type
    /// dispatch matches the server via [`paths::make_socket_name`].
    pub fn connect(scope_hash: Option<&str>) -> Result<Self, ClientError> {
        let path = paths::socket_path(scope_hash);
        Self::connect_to(&path)
    }

    /// Connect to a daemon listening at an explicit socket path.
    ///
    /// Use this when you already know the socket path (e.g. in integration
    /// tests that start a server on a unique path).
    pub fn connect_to(socket_path: &str) -> Result<Self, ClientError> {
        let name = paths::make_socket_name(socket_path).map_err(ClientError::Connect)?;

        use interprocess::local_socket::traits::Stream as _;
        let stream = Stream::connect(name).map_err(ClientError::Connect)?;
        let stream_clone = stream.try_clone().map_err(ClientError::Connect)?;

        Ok(Self {
            reader: BufReader::new(stream),
            writer: BufWriter::new(stream_clone),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a request and wait for the corresponding response.
    ///
    /// The request is length-prefixed (4-byte big-endian u32) then protobuf-encoded.
    /// The response uses the same framing.
    pub fn send_request(
        &mut self,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, ClientError> {
        // Encode
        let payload = request.encode_to_vec();
        let len = payload.len() as u32;

        // Write length prefix + payload
        self.writer
            .write_all(&len.to_be_bytes())
            .map_err(ClientError::Io)?;
        self.writer
            .write_all(&payload)
            .map_err(ClientError::Io)?;
        self.writer.flush().map_err(ClientError::Io)?;

        // Read length prefix
        let mut len_buf = [0u8; 4];
        self.reader
            .read_exact(&mut len_buf)
            .map_err(ClientError::Io)?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;

        // Read payload
        let mut resp_buf = vec![0u8; resp_len];
        self.reader
            .read_exact(&mut resp_buf)
            .map_err(ClientError::Io)?;

        DaemonResponse::decode(&resp_buf[..]).map_err(ClientError::Decode)
    }

    // -----------------------------------------------------------------------
    // Convenience helpers
    // -----------------------------------------------------------------------

    /// Allocate the next request ID.
    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Ping the daemon to check liveness.
    pub fn ping(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Ping.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            ping: Some(PingRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Ask the daemon to shut down.
    pub fn shutdown(
        &mut self,
        graceful: bool,
        timeout_seconds: f64,
    ) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Shutdown.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            shutdown: Some(ShutdownRequest {
                graceful,
                timeout_seconds,
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Query daemon status.
    pub fn status(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Status.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            status: Some(StatusRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// List all active tracked processes.
    pub fn list_active(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListActive.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            list_active: Some(ListActiveRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// List tracked processes filtered by originator tool name.
    pub fn list_by_originator(&mut self, tool: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListByOriginator.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            list_by_originator: Some(ListByOriginatorRequest {
                tool: tool.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Kill zombie processes tracked by the daemon.
    pub fn kill_zombies(&mut self, dry_run: bool) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::KillZombies.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            kill_zombies: Some(KillZombiesRequest { dry_run }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Kill a process tree rooted at `pid`.
    pub fn kill_tree(
        &mut self,
        pid: u32,
        timeout_seconds: f64,
    ) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::KillTree.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            kill_tree: Some(KillTreeRequest {
                pid,
                timeout_seconds,
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Get the process tree display for a given PID.
    pub fn get_process_tree(&mut self, pid: u32) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetProcessTree.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            get_process_tree: Some(GetProcessTreeRequest { pid }),
            ..Default::default()
        };
        self.send_request(request)
    }
}

// ---------------------------------------------------------------------------
// Auto-start logic
// ---------------------------------------------------------------------------

/// Connect to the daemon, starting it first if it is not running.
///
/// 1. Attempt to connect.
/// 2. On failure, spawn `running-process-daemon start` as a detached process.
/// 3. Retry with exponential back-off: 50 ms, 100 ms, 200 ms, 400 ms.
/// 4. Return an error if the daemon cannot be reached after all retries.
pub fn connect_or_start(scope_hash: Option<&str>) -> Result<DaemonClient, ClientError> {
    // Fast path: daemon already running.
    if let Ok(client) = DaemonClient::connect(scope_hash) {
        return Ok(client);
    }

    // Spawn the daemon as a detached background process.
    spawn_daemon()?;

    // Retry with exponential back-off.
    let delays_ms: [u64; 4] = [50, 100, 200, 400];
    for delay in delays_ms {
        std::thread::sleep(std::time::Duration::from_millis(delay));
        if let Ok(client) = DaemonClient::connect(scope_hash) {
            return Ok(client);
        }
    }

    Err(ClientError::DaemonNotRunning)
}

/// Spawn the daemon binary as a detached background process.
fn spawn_daemon() -> Result<(), ClientError> {
    let exe = daemon_exe_path();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0200;
        std::process::Command::new(&exe)
            .arg("start")
            .creation_flags(DETACHED)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(ClientError::Io)?;
    }

    #[cfg(unix)]
    {
        std::process::Command::new(&exe)
            .arg("start")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(ClientError::Io)?;
    }

    Ok(())
}

/// Determine the path to the daemon executable.
///
/// Looks next to the current executable first, then falls back to expecting
/// it on `$PATH`.
fn daemon_exe_path() -> String {
    if let Ok(mut path) = std::env::current_exe() {
        path.pop(); // remove current binary name
        let candidate = path.join(if cfg!(windows) {
            "running-process-daemon.exe"
        } else {
            "running-process-daemon"
        });
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    // Fallback: assume it is on PATH.
    String::from("running-process-daemon")
}
