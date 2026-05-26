//! Client helpers for daemon-owned session tee telemetry.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

use crate::client::{ClientError, DaemonClient};
use crate::proto::daemon::{
    DaemonRequest, GetSessionTeeStatusRequest, RegisterSessionTeeRequest, RequestType, StatusCode,
    TeeBackpressure as ProtoTeeBackpressure, TeeFileMode as ProtoTeeFileMode,
    TeeSessionKind as ProtoTeeSessionKind, TeeSinkKind, TeeStreamKind as ProtoTeeStreamKind,
    UnregisterSessionTeeRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTeeKind {
    Pty,
    Pipe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTeeStream {
    PtyOutput,
    Stdout,
    Stderr,
    Stdin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTeeFileMode {
    Append,
    Truncate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTeeBackpressure {
    DropOldest,
    Block,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTeeFileRequest {
    pub session_id: String,
    pub session_kind: SessionTeeKind,
    pub stream: SessionTeeStream,
    pub path: PathBuf,
    pub mode: SessionTeeFileMode,
    /// 0 means use the daemon default.
    pub queue_capacity: u32,
    pub write_missed_markers: bool,
    pub backpressure: SessionTeeBackpressure,
}

impl SessionTeeFileRequest {
    pub fn new<P>(
        session_id: impl Into<String>,
        session_kind: SessionTeeKind,
        stream: SessionTeeStream,
        path: P,
    ) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            session_id: session_id.into(),
            session_kind,
            stream,
            path: path.as_ref().to_path_buf(),
            mode: SessionTeeFileMode::Append,
            queue_capacity: 0,
            write_missed_markers: true,
            backpressure: SessionTeeBackpressure::DropOldest,
        }
    }

    pub fn truncate(mut self) -> Self {
        self.mode = SessionTeeFileMode::Truncate;
        self
    }

    pub fn queue_capacity(mut self, capacity: u32) -> Self {
        self.queue_capacity = capacity;
        self
    }

    pub fn suppress_missed_markers(mut self) -> Self {
        self.write_missed_markers = false;
        self
    }

    pub fn backpressure(mut self, backpressure: SessionTeeBackpressure) -> Self {
        self.backpressure = backpressure;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionTeeStatus {
    pub stream: SessionTeeStream,
    pub missed_bytes: u64,
    pub disconnected: bool,
}

impl DaemonClient {
    pub fn register_session_file_tee(
        &mut self,
        request: &SessionTeeFileRequest,
    ) -> Result<u64, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::RegisterSessionTee.into(),
            protocol_version: 1,
            register_session_tee: Some(RegisterSessionTeeRequest {
                session_id: request.session_id.clone(),
                session_kind: proto_session_kind(request.session_kind) as i32,
                stream: proto_stream_kind(request.stream) as i32,
                sink_kind: TeeSinkKind::File as i32,
                file_path: encode_os_path(&request.path),
                file_mode: proto_file_mode(request.mode) as i32,
                queue_capacity: request.queue_capacity,
                suppress_missed_markers: !request.write_missed_markers,
                backpressure: proto_backpressure(request.backpressure) as i32,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .register_session_tee
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "register_session_tee response missing payload".into(),
            })?;
        Ok(payload.tee_handle)
    }

    pub fn unregister_session_tee(
        &mut self,
        session_kind: SessionTeeKind,
        session_id: &str,
        tee_handle: u64,
    ) -> Result<(), ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::UnregisterSessionTee.into(),
            protocol_version: 1,
            unregister_session_tee: Some(UnregisterSessionTeeRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                tee_handle,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)
    }

    pub fn get_session_tee_status(
        &mut self,
        session_kind: SessionTeeKind,
        session_id: &str,
        tee_handle: u64,
    ) -> Result<SessionTeeStatus, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetSessionTeeStatus.into(),
            protocol_version: 1,
            get_session_tee_status: Some(GetSessionTeeStatusRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                tee_handle,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .get_session_tee_status
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "get_session_tee_status response missing payload".into(),
            })?;
        let stream =
            ProtoTeeStreamKind::try_from(payload.stream).map_err(|_| ClientError::Server {
                code: StatusCode::Internal,
                message: "get_session_tee_status response has invalid stream".into(),
            })?;
        Ok(SessionTeeStatus {
            stream: client_stream_kind(stream)?,
            missed_bytes: payload.missed_bytes,
            disconnected: payload.disconnected,
        })
    }
}

fn ensure_ok(response: &crate::proto::daemon::DaemonResponse) -> Result<(), ClientError> {
    if response.code == StatusCode::Ok as i32 {
        return Ok(());
    }
    let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
    Err(ClientError::Server {
        code,
        message: response.message.clone(),
    })
}

fn proto_session_kind(kind: SessionTeeKind) -> ProtoTeeSessionKind {
    match kind {
        SessionTeeKind::Pty => ProtoTeeSessionKind::Pty,
        SessionTeeKind::Pipe => ProtoTeeSessionKind::Pipe,
    }
}

fn proto_stream_kind(stream: SessionTeeStream) -> ProtoTeeStreamKind {
    match stream {
        SessionTeeStream::PtyOutput => ProtoTeeStreamKind::PtyOutput,
        SessionTeeStream::Stdout => ProtoTeeStreamKind::Stdout,
        SessionTeeStream::Stderr => ProtoTeeStreamKind::Stderr,
        SessionTeeStream::Stdin => ProtoTeeStreamKind::Stdin,
    }
}

fn client_stream_kind(stream: ProtoTeeStreamKind) -> Result<SessionTeeStream, ClientError> {
    match stream {
        ProtoTeeStreamKind::PtyOutput => Ok(SessionTeeStream::PtyOutput),
        ProtoTeeStreamKind::Stdout => Ok(SessionTeeStream::Stdout),
        ProtoTeeStreamKind::Stderr => Ok(SessionTeeStream::Stderr),
        ProtoTeeStreamKind::Stdin => Ok(SessionTeeStream::Stdin),
        ProtoTeeStreamKind::Unspecified => Err(ClientError::Server {
            code: StatusCode::Internal,
            message: "get_session_tee_status response has unspecified stream".into(),
        }),
    }
}

fn proto_file_mode(mode: SessionTeeFileMode) -> ProtoTeeFileMode {
    match mode {
        SessionTeeFileMode::Append => ProtoTeeFileMode::Append,
        SessionTeeFileMode::Truncate => ProtoTeeFileMode::Truncate,
    }
}

fn proto_backpressure(backpressure: SessionTeeBackpressure) -> ProtoTeeBackpressure {
    match backpressure {
        SessionTeeBackpressure::DropOldest => ProtoTeeBackpressure::DropOldest,
        SessionTeeBackpressure::Block => ProtoTeeBackpressure::Block,
    }
}

#[cfg(unix)]
fn encode_os_path(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn encode_os_path(path: &Path) -> Vec<u8> {
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
