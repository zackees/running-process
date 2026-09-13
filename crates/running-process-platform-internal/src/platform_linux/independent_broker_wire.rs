//! Protobuf framing for the explicit Linux broker; no product daemon payloads.
use crate::platform::independent_spawn::{read_exact, write_all, LaunchSpec, Readiness, MAX_FRAME};
use prost::Message;
pub(super) use running_process_protocol::independent_spawn::{self as wire, frame::Body};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    sync::atomic::AtomicBool,
    time::Instant,
};

pub(super) fn send(
    stream: &mut impl Write,
    body: Body,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let frame = wire::Frame { body: Some(body) };
    let length = frame.encoded_len();
    if length > MAX_FRAME {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let bytes = frame.encode_to_vec();
    write_all(stream, &(length as u32).to_le_bytes(), deadline, cancelled)?;
    write_all(stream, &bytes, deadline, cancelled)
}

pub(super) fn receive(
    stream: &mut impl Read,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Body> {
    let mut length = [0_u8; 4];
    read_exact(stream, &mut length, deadline, cancelled)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut bytes = vec![0; length];
    read_exact(stream, &mut bytes, deadline, cancelled)?;
    wire::Frame::decode(bytes.as_slice())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
        .body
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

pub(super) fn encode_spec(spec: &LaunchSpec) -> wire::Launch {
    let bytes = |value: &OsString| value.as_os_str().as_bytes().to_vec();
    wire::Launch {
        timeout_millis: 30000,
        program: bytes(&spec.program),
        args: spec.args.iter().map(bytes).collect(),
        cwd: bytes(&spec.cwd),
        environment: spec
            .environment
            .iter()
            .map(|(key, value)| wire::EnvironmentEntry {
                name: bytes(key),
                value: bytes(value),
            })
            .collect(),
        stdout: spec.stdout.as_ref().map(bytes),
        stderr: spec.stderr.as_ref().map(bytes),
        readiness: match &spec.readiness {
            Readiness::ProcessStarted => None,
            Readiness::File { path, value } => Some(wire::ReadyFile {
                path: bytes(path),
                value: value.clone(),
            }),
        },
    }
}

pub(super) fn decode_spec(spec: wire::Launch) -> LaunchSpec {
    LaunchSpec {
        program: OsString::from_vec(spec.program),
        args: spec.args.into_iter().map(OsString::from_vec).collect(),
        cwd: OsString::from_vec(spec.cwd),
        environment: spec
            .environment
            .into_iter()
            .map(|entry| {
                (
                    OsString::from_vec(entry.name),
                    OsString::from_vec(entry.value),
                )
            })
            .collect(),
        stdout: spec.stdout.map(OsString::from_vec),
        stderr: spec.stderr.map(OsString::from_vec),
        readiness: spec
            .readiness
            .map_or(Readiness::ProcessStarted, |file| Readiness::File {
                path: OsString::from_vec(file.path),
                value: file.value,
            }),
    }
}

pub(super) fn failure(error: &io::Error) -> wire::Failure {
    match error.kind() {
        io::ErrorKind::Unsupported => wire::Failure::Unsupported,
        io::ErrorKind::PermissionDenied => wire::Failure::PermissionDenied,
        io::ErrorKind::InvalidInput => wire::Failure::InvalidInput,
        io::ErrorKind::NotFound => wire::Failure::NotFound,
        io::ErrorKind::TimedOut => wire::Failure::TimedOut,
        io::ErrorKind::Interrupted => wire::Failure::Cancelled,
        _ => wire::Failure::LaunchFailed,
    }
}

pub(super) fn failure_into_io(code: i32) -> io::Error {
    let kind = match wire::Failure::try_from(code) {
        Ok(wire::Failure::Unsupported) => io::ErrorKind::Unsupported,
        Ok(wire::Failure::PermissionDenied) => io::ErrorKind::PermissionDenied,
        Ok(wire::Failure::InvalidInput) => io::ErrorKind::InvalidInput,
        Ok(wire::Failure::NotFound) => io::ErrorKind::NotFound,
        Ok(wire::Failure::TimedOut) => io::ErrorKind::TimedOut,
        Ok(wire::Failure::Cancelled) => io::ErrorKind::Interrupted,
        Ok(wire::Failure::LaunchFailed) => io::ErrorKind::Other,
        Err(_) => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, "external broker launch failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn broker_wire_preserves_failure_categories() {
        for kind in [
            io::ErrorKind::Unsupported,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::NotFound,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
            io::ErrorKind::Other,
        ] {
            let deadline = Instant::now() + Duration::from_secs(1);
            let cancelled = AtomicBool::new(false);
            let mut bytes = Vec::new();
            send(
                &mut bytes,
                Body::Failed(failure(&io::Error::from(kind)) as i32),
                deadline,
                &cancelled,
            )
            .unwrap();
            let Body::Failed(code) = receive(&mut bytes.as_slice(), deadline, &cancelled).unwrap()
            else {
                panic!("expected failure")
            };
            assert_eq!(failure_into_io(code).kind(), kind);
        }
        assert_eq!(failure_into_io(1000).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn broker_wire_preserves_native_strings() {
        let native = OsString::from_vec(vec![b'a', b' ', 0xff, b'\'']);
        let spec = LaunchSpec {
            program: "/bin/target".into(),
            args: vec![native.clone()],
            cwd: "/tmp".into(),
            environment: vec![("SELECTED".into(), native.clone())],
            stdout: Some("/tmp/out".into()),
            stderr: None,
            readiness: Readiness::ProcessStarted,
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let cancelled = AtomicBool::new(false);
        let mut bytes = Vec::new();
        send(
            &mut bytes,
            Body::Launch(encode_spec(&spec)),
            deadline,
            &cancelled,
        )
        .unwrap();
        let Body::Launch(payload) = receive(&mut bytes.as_slice(), deadline, &cancelled).unwrap()
        else {
            panic!("expected launch")
        };
        let decoded = decode_spec(payload);
        assert_eq!(decoded.args, vec![native.clone()]);
        assert_eq!(decoded.environment, vec![("SELECTED".into(), native)]);
        assert_eq!(decoded.stdout, spec.stdout);
        assert_eq!(decoded.stderr, None);
    }
}
