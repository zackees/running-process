//! Client-side helpers for daemon-owned pipe-backed sessions
//! (issue #130 milestone 3).
//!
//! Mirrors [`crate::pty_session`] for the pipe case. Sessions are spawned,
//! listed, detached, and terminated via the regular [`DaemonClient`] RPC
//! channel. Stdin is also an RPC (`write_pipe_stdin`). Stdout/stderr are
//! attached via [`PipeStreamAttachment`], which owns its own connection
//! and pumps `PipeStreamFrame` payloads.

use crate::client::{ClientError, DaemonClient};
use crate::paths;
use interprocess::local_socket::Stream;
use interprocess::TryClone;
use prost::Message;
use running_process::proto::daemon::{
    AttachPipeStreamRequest, AttachPipeStreamResponse, DaemonRequest, DaemonResponse,
    DetachPipeStreamRequest, KeyValue, ListPipeSessionsRequest, ListPipeSessionsResponse,
    PipeSessionInfo, PipeStreamFrame, PipeStreamKind, RequestType, SpawnPipeSessionRequest,
    SpawnPipeSessionResponse, StatusCode, TerminatePipeSessionRequest, WritePipeStdinRequest,
    WritePipeStdinResponse,
};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Spawn / list / terminate / write helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PipeSpawnRequest {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub clear_inherited_env: bool,
    pub originator: Option<String>,
    pub merge_stderr_into_stdout: bool,
}

impl PipeSpawnRequest {
    pub fn new<S: Into<String>>(argv: impl IntoIterator<Item = S>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
            clear_inherited_env: false,
            originator: None,
            merge_stderr_into_stdout: false,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = Some(originator.into());
        self
    }

    pub fn merge_stderr(mut self) -> Self {
        self.merge_stderr_into_stdout = true;
        self
    }

    pub fn with_envs<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }
}

#[derive(Debug, Clone)]
pub struct SpawnedPipeSession {
    pub session_id: String,
    pub pid: u32,
    pub created_at: f64,
}

impl DaemonClient {
    pub fn spawn_pipe_session(
        &mut self,
        request: &PipeSpawnRequest,
    ) -> Result<SpawnedPipeSession, ClientError> {
        let proto = SpawnPipeSessionRequest {
            argv: request.argv.clone(),
            cwd: request
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            env: request
                .env
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect(),
            clear_inherited_env: request.clear_inherited_env,
            originator: request.originator.clone().unwrap_or_default(),
            merge_stderr_into_stdout: request.merge_stderr_into_stdout,
        };
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::SpawnPipeSession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            spawn_pipe_session: Some(proto),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload: SpawnPipeSessionResponse = response
            .spawn_pipe_session
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "spawn_pipe_session response missing payload".into(),
            })?;
        Ok(SpawnedPipeSession {
            session_id: payload.session_id,
            pid: payload.pid,
            created_at: payload.created_at,
        })
    }

    pub fn list_pipe_sessions(
        &mut self,
        originator_filter: &str,
    ) -> Result<Vec<PipeSessionInfo>, ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListPipeSessions.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            list_pipe_sessions: Some(ListPipeSessionsRequest {
                originator: originator_filter.into(),
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        let payload: ListPipeSessionsResponse = response
            .list_pipe_sessions
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "list_pipe_sessions response missing payload".into(),
            })?;
        Ok(payload.sessions)
    }

    pub fn detach_pipe_stream(
        &mut self,
        session_id: &str,
        stream: PipeStreamKind,
    ) -> Result<(), ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::DetachPipeStream.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            detach_pipe_stream: Some(DetachPipeStreamRequest {
                session_id: session_id.into(),
                stream: stream as i32,
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        Ok(())
    }

    pub fn terminate_pipe_session(
        &mut self,
        session_id: &str,
        grace_ms: u32,
    ) -> Result<(), ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::TerminatePipeSession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            terminate_pipe_session: Some(TerminatePipeSessionRequest {
                session_id: session_id.into(),
                grace_ms,
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        Ok(())
    }

    pub fn write_pipe_stdin(
        &mut self,
        session_id: &str,
        data: &[u8],
        close_after: bool,
    ) -> Result<u64, ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::WritePipeStdin.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            write_pipe_stdin: Some(WritePipeStdinRequest {
                session_id: session_id.into(),
                data: data.to_vec(),
                close: close_after,
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        let payload: WritePipeStdinResponse = response
            .write_pipe_stdin
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "write_pipe_stdin response missing payload".into(),
            })?;
        Ok(payload.bytes_written)
    }
}

fn ensure_ok(response: &DaemonResponse) -> Result<(), ClientError> {
    if response.code == StatusCode::Ok as i32 {
        return Ok(());
    }
    let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
    Err(ClientError::Server {
        code,
        message: response.message.clone(),
    })
}

// ---------------------------------------------------------------------------
// PipeStreamAttachment
// ---------------------------------------------------------------------------

pub struct PipeStreamAttachment {
    reader: BufReader<Stream>,
    pub initial_backlog: Vec<u8>,
    pub bytes_missed: u64,
}

#[derive(Debug)]
pub enum PipeAttachError {
    Connect(std::io::Error),
    Io(std::io::Error),
    Decode(prost::DecodeError),
    Server { code: StatusCode, message: String },
    MissingPayload,
}

impl std::fmt::Display for PipeAttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "pipe attach connect failed: {e}"),
            Self::Io(e) => write!(f, "pipe attach io error: {e}"),
            Self::Decode(e) => write!(f, "pipe attach decode error: {e}"),
            Self::Server { code, message } => {
                write!(f, "pipe attach server error {code:?}: {message}")
            }
            Self::MissingPayload => write!(f, "pipe attach response missing payload"),
        }
    }
}

impl std::error::Error for PipeAttachError {}

impl PipeStreamAttachment {
    pub fn attach(
        scope_hash: Option<&str>,
        session_id: &str,
        stream: PipeStreamKind,
        steal: bool,
    ) -> Result<Self, PipeAttachError> {
        let socket_path = paths::socket_path(scope_hash);
        Self::attach_to(&socket_path, session_id, stream, steal)
    }

    pub fn attach_to(
        socket_path: &str,
        session_id: &str,
        stream: PipeStreamKind,
        steal: bool,
    ) -> Result<Self, PipeAttachError> {
        let name = paths::make_socket_name(socket_path).map_err(PipeAttachError::Connect)?;
        use interprocess::local_socket::traits::Stream as _;
        let s = Stream::connect(name).map_err(PipeAttachError::Connect)?;
        let s_clone = s.try_clone().map_err(PipeAttachError::Connect)?;
        let mut reader = BufReader::new(s);
        let mut writer = BufWriter::new(s_clone);

        let attach_request = DaemonRequest {
            id: 1,
            r#type: RequestType::AttachPipeStream.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            attach_pipe_stream: Some(AttachPipeStreamRequest {
                session_id: session_id.into(),
                stream: stream as i32,
                steal,
            }),
            ..Default::default()
        };
        write_length_prefixed(&mut writer, &attach_request.encode_to_vec())
            .map_err(PipeAttachError::Io)?;
        // We do not need writer after this, but keep it alive via reader's
        // duplex socket. Drop here.
        drop(writer);

        let response_bytes = read_length_prefixed(&mut reader).map_err(PipeAttachError::Io)?;
        let response = DaemonResponse::decode(&response_bytes[..]).map_err(PipeAttachError::Decode)?;
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(PipeAttachError::Server {
                code,
                message: response.message,
            });
        }
        let payload: AttachPipeStreamResponse = response
            .attach_pipe_stream
            .ok_or(PipeAttachError::MissingPayload)?;

        Ok(Self {
            reader,
            initial_backlog: payload.backlog,
            bytes_missed: payload.bytes_missed,
        })
    }

    pub fn recv_frame(&mut self) -> Result<PipeStreamFrame, PipeAttachError> {
        let bytes = read_length_prefixed(&mut self.reader).map_err(PipeAttachError::Io)?;
        PipeStreamFrame::decode(&bytes[..]).map_err(PipeAttachError::Decode)
    }
}

fn write_length_prefixed<W: Write>(w: &mut W, payload: &[u8]) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_length_prefixed<R: Read>(r: &mut R) -> Result<Vec<u8>, std::io::Error> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}
