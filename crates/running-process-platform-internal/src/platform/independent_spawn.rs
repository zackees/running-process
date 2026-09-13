//! Private-launcher protocol and target supervision shared by native schedulers.
//! Application payloads travel over authenticated owner-private IPC, not task
//! registration metadata. The uncommitted helper has a bounded lifetime.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::ipc::{current_user_id, Endpoint, Stream};
use super::process::{SpawnStdio, StdioSource, SyncEnvironment};
use serde::{Deserialize, Serialize};

const MAX_FRAME: usize = 1024 * 1024;
pub use crate::{independent_spawn as spawn, IndependentChild};
pub(crate) const LEASE: Duration = Duration::from_secs(30);

/// Normalize the selected transport's empty nonblocking read convention.
/// Windows named pipes may report zero while pending; Unix sockets use zero
/// for EOF. Keep that host choice in the existing IPC capability.
pub(crate) struct Channel(Stream);
impl Channel {
    pub(crate) fn new(stream: Stream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }
}
impl Read for Channel {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.0.read(bytes)?;
        if count == 0 && !bytes.is_empty() && crate::ipc_nonblocking_zero_read_is_pending() {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(count)
        }
    }
}
impl Write for Channel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Explicit target payload. No inherited environment or caller-owned pipe is
/// admitted by this scheduler contract. Native strings retain Unix bytes and
/// Windows UTF-16 code units through the private serde representation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: OsString,
    pub environment: Vec<(OsString, OsString)>,
    pub stdout: Option<OsString>,
    pub stderr: Option<OsString>,
}

impl LaunchSpec {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.args.len() > 4096 || self.environment.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many launch arguments or environment entries",
            ));
        }
        let strings = std::iter::once(&self.program)
            .chain(std::iter::once(&self.cwd))
            .chain(self.args.iter())
            .chain(
                self.environment
                    .iter()
                    .flat_map(|(key, value)| [key, value]),
            )
            .chain(self.stdout.iter())
            .chain(self.stderr.iter());
        let mut total = 0_usize;
        for value in strings {
            let bytes = value.as_encoded_bytes();
            total = total.saturating_add(bytes.len());
            if bytes.contains(&0) || total > MAX_FRAME / 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid or oversized native launch strings",
                ));
            }
        }
        if self
            .environment
            .iter()
            .any(|(key, _)| key.is_empty() || key.as_encoded_bytes().contains(&b'='))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment name",
            ));
        }
        if !PathBuf::from(&self.program).is_absolute()
            || !PathBuf::from(&self.cwd).is_absolute()
            || self
                .stdout
                .iter()
                .chain(self.stderr.iter())
                .any(|path| !PathBuf::from(path).is_absolute())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "launch paths must be absolute",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Message {
    Launch(LaunchSpec),
    Started { pid: u32 },
    Commit,
    Committed,
    Failed { kind: FailureKind },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum FailureKind {
    Unsupported,
    PermissionDenied,
    InvalidInput,
    NotFound,
    LaunchFailed,
}

impl FailureKind {
    pub(crate) fn into_io(self) -> io::Error {
        let kind = match self {
            Self::Unsupported => io::ErrorKind::Unsupported,
            Self::PermissionDenied => io::ErrorKind::PermissionDenied,
            Self::InvalidInput => io::ErrorKind::InvalidInput,
            Self::NotFound => io::ErrorKind::NotFound,
            Self::LaunchFailed => io::ErrorKind::Other,
        };
        io::Error::new(kind, "independent target launch failed")
    }
    fn from_io(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::NotFound => Self::NotFound,
            _ => Self::LaunchFailed,
        }
    }
}

struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "launch payload exceeds limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn check(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "launch cancelled",
        ))
    } else if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "launch handshake deadline expired",
        ))
    } else {
        Ok(())
    }
}

fn retry(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    check(deadline, cancelled)?;
    std::thread::sleep(
        Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
    );
    Ok(())
}

fn write_all(
    stream: &mut impl Write,
    mut bytes: &[u8],
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        check(deadline, cancelled)?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                retry(deadline, cancelled)?
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_exact(
    stream: &mut impl Read,
    mut bytes: &mut [u8],
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        check(deadline, cancelled)?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                retry(deadline, cancelled)?
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn send(
    stream: &mut impl Write,
    message: &Message,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let mut encoded = Limited(Vec::new());
    serde_json::to_writer(&mut encoded, message).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid or oversized launch payload",
        )
    })?;
    write_all(
        stream,
        &(encoded.0.len() as u32).to_le_bytes(),
        deadline,
        cancelled,
    )?;
    write_all(stream, &encoded.0, deadline, cancelled)
}

pub(crate) fn receive(
    stream: &mut impl Read,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Message> {
    let mut length = [0_u8; 4];
    read_exact(stream, &mut length, deadline, cancelled)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid launch frame length",
        ));
    }
    let mut bytes = vec![0; length];
    read_exact(stream, &mut bytes, deadline, cancelled)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid launch frame"))
}

/// Entry point for the installed helper binary. An absent or disconnected
/// caller cannot leave a target behind: target ownership remains in the helper
/// until commit, and the scheduler owns the helper's entire service lifetime.
pub fn run_launcher(endpoint: &str) -> io::Result<i32> {
    let deadline = Instant::now() + LEASE;
    let cancelled = AtomicBool::new(false);
    let endpoint = Endpoint::new(endpoint)?;
    let stream = Stream::connect(&endpoint)?;
    if stream.peer_identity()?.user_id != current_user_id()? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let mut stream = Channel::new(stream)?;
    let Message::Launch(spec) = receive(&mut stream, deadline, &cancelled)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected launch payload",
        ));
    };
    let mut child = match spawn_target(&spec) {
        Ok(child) => child,
        Err(error) => {
            let _ = send(
                &mut stream,
                &Message::Failed {
                    kind: FailureKind::from_io(&error),
                },
                deadline,
                &cancelled,
            );
            return Err(io::Error::new(error.kind(), "target launch failed"));
        }
    };
    send(
        &mut stream,
        &Message::Started { pid: child.id() },
        deadline,
        &cancelled,
    )?;
    if !matches!(receive(&mut stream, deadline, &cancelled)?, Message::Commit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected launch commit",
        ));
    }
    send(&mut stream, &Message::Committed, deadline, &cancelled)?;
    drop(stream);
    child.wait()
}

fn spawn_target(spec: &LaunchSpec) -> io::Result<super::process::SpawnedChild> {
    spec.validate()?;
    let open_log = |path: &Option<OsString>| -> io::Result<Option<std::fs::File>> {
        path.as_ref()
            .map(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            })
            .transpose()
    };
    let stdout = open_log(&spec.stdout)?;
    let stderr = open_log(&spec.stderr)?;
    let mut command = std::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.environment.iter().cloned());
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: stdout
            .as_ref()
            .map(StdioSource::File)
            .unwrap_or(StdioSource::Null),
        stderr: stderr
            .as_ref()
            .map(StdioSource::File)
            .unwrap_or(StdioSource::Null),
        drain_timeout: None,
        show_console: false,
    };
    crate::spawn_sync(&mut command, stdio, SyncEnvironment::Explicit(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_frame_is_rejected_before_body_allocation() {
        let mut frame = io::Cursor::new(u32::MAX.to_le_bytes());
        assert_eq!(
            receive(&mut frame, Instant::now() + LEASE, &AtomicBool::new(false))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn disconnected_peer_fails_before_launch() {
        assert_eq!(
            receive(
                &mut io::empty(),
                Instant::now() + LEASE,
                &AtomicBool::new(false)
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn payload_round_trip_preserves_argument_boundaries() {
        let spec = LaunchSpec {
            program: "/program".into(),
            args: vec!["".into(), "a b".into(), "$SECRET;*".into()],
            cwd: "/cwd".into(),
            environment: vec![("KEY".into(), "literal value".into())],
            stdout: None,
            stderr: None,
        };
        let mut bytes = Vec::new();
        send(
            &mut bytes,
            &Message::Launch(spec.clone()),
            Instant::now() + LEASE,
            &AtomicBool::new(false),
        )
        .unwrap();
        let Message::Launch(actual) = receive(
            &mut io::Cursor::new(bytes),
            Instant::now() + LEASE,
            &AtomicBool::new(false),
        )
        .unwrap() else {
            panic!("wrong message");
        };
        assert_eq!(actual.args, spec.args);
        assert_eq!(actual.environment, spec.environment);
    }
}
