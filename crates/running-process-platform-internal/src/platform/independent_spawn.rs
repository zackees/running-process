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
        let count = self.0.write(bytes)?;
        // PIPE_NOWAIT byte pipes can succeed with zero bytes when their
        // buffer is full. Retry under the codec deadline, not as WriteZero.
        if count == 0 && !bytes.is_empty() && crate::INDEPENDENT_ZERO_WRITE_PENDING {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(count)
        }
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
    #[serde(default)]
    pub readiness: Readiness,
}

/// Application readiness is distinct from successful exec. A file marker must
/// be absent before launch and contain exactly the caller-selected bytes.
/// The caller owns the marker's path and eventual removal.
#[derive(Clone, Default, Serialize, Deserialize)]
pub enum Readiness {
    #[default]
    ProcessStarted,
    File {
        path: OsString,
        value: Vec<u8>,
    },
}

impl LaunchSpec {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if let Readiness::File { path, value } = &self.readiness {
            if !PathBuf::from(path).is_absolute()
                || path.as_encoded_bytes().contains(&0)
                || path.len() > 4096
                || value.is_empty()
                || value.len() > 4096
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid readiness marker",
                ));
            }
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "readiness marker already exists",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
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

pub(crate) fn is_ready(readiness: &Readiness) -> io::Result<bool> {
    match readiness {
        Readiness::ProcessStarted => Ok(true),
        Readiness::File { path, value } => {
            let file = match crate::independent_open_regular(std::path::Path::new(path), false) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            let mut bytes = Vec::new();
            file.take(value.len() as u64 + 1).read_to_end(&mut bytes)?;
            Ok(bytes == *value)
        }
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

type PreparedTarget = (
    std::process::Command,
    Option<std::fs::File>,
    Option<std::fs::File>,
);

fn prepare_target(spec: &LaunchSpec) -> io::Result<PreparedTarget> {
    spec.validate()?;
    let open_log = |path: &Option<OsString>| -> io::Result<Option<std::fs::File>> {
        path.as_ref()
            .map(|path| crate::independent_open_regular(std::path::Path::new(path), true))
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
    Ok((command, stdout, stderr))
}

fn spawn_target(spec: &LaunchSpec) -> io::Result<super::process::SpawnedChild> {
    spawn_owned_target(spec, false)
}

fn spawn_owned_target(
    spec: &LaunchSpec,
    detached: bool,
) -> io::Result<super::process::SpawnedChild> {
    let (mut command, stdout, stderr) = prepare_target(spec)?;
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
    if detached {
        crate::spawn_sync_owned_daemon(&mut command, stdio, SyncEnvironment::Explicit(Vec::new()))
    } else {
        crate::spawn_sync(&mut command, stdio, SyncEnvironment::Explicit(Vec::new()))
    }
}

/// Private-substrate direct child, using the existing sanitized spawn engines.
pub struct InheritedChild {
    child: super::process::SpawnedChild,
    committed: bool,
}
impl InheritedChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.child.try_wait()
    }
    pub fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        self.child.kill_tree()?;
        self.wait(timeout, &AtomicBool::new(false)).map(|_| ())
    }
    pub fn wait(&mut self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<i32> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(code);
            }
            retry(deadline, cancelled)?;
        }
    }
}
impl Drop for InheritedChild {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.stop(Duration::from_secs(2));
        }
    }
}

/// Direct placement with explicit payload and the same readiness budget.
pub fn spawn_inherited(
    spec: &LaunchSpec,
    detached: bool,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> io::Result<InheritedChild> {
    if timeout.is_zero() || timeout > LEASE {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let deadline = Instant::now() + timeout;
    check(deadline, cancelled)?;
    let mut child = spawn_owned_target(spec, detached)?;
    child.retain_exit_identity();
    let mut child = InheritedChild {
        child,
        committed: false,
    };
    while !is_ready(&spec.readiness)? {
        check(deadline, cancelled)?;
        if child.try_wait()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "target exited before readiness",
            ));
        }
        retry(deadline, cancelled)?;
    }
    check(deadline, cancelled)?;
    if detached {
        child.child.commit_detached()?;
    }
    child.committed = true;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_large_frame_round_trip() {
        let endpoint = Endpoint::test("independent-frame").unwrap();
        let listener = super::super::ipc::Listener::bind(&endpoint).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let server = std::thread::spawn(move || {
            let mut channel = Channel::new(listener.accept().unwrap()).unwrap();
            let message = receive(&mut channel, deadline, &AtomicBool::new(false)).unwrap();
            let Message::Launch(spec) = message else {
                panic!("expected launch");
            };
            assert_eq!(spec.args, vec![OsString::from("x".repeat(8192))]);
            send(
                &mut channel,
                &Message::Committed,
                deadline,
                &AtomicBool::new(false),
            )
            .unwrap();
        });
        let mut channel = Channel::new(Stream::connect(&endpoint).unwrap()).unwrap();
        let spec = LaunchSpec {
            program: "fixture".into(),
            args: vec!["x".repeat(8192).into()],
            cwd: ".".into(),
            environment: vec![],
            stdout: None,
            stderr: None,
            readiness: Readiness::ProcessStarted,
        };
        send(
            &mut channel,
            &Message::Launch(spec),
            deadline,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(matches!(
            receive(&mut channel, deadline, &AtomicBool::new(false)).unwrap(),
            Message::Committed
        ));
        server.join().unwrap();
        endpoint.retire().unwrap();
    }

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
            readiness: Readiness::ProcessStarted,
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
