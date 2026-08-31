//! Real-process backend fixture for broker lifecycle conformance tests.
//!
//! This binary is gated behind the off-by-default `test-seams` feature. It
//! consumes the same launch environment as a third-party backend, answers
//! production identity probes through `BackendEndpointMux`, and serves one
//! private payload protocol carrying its PID. Keeping the fixture generic
//! lets lifecycle tests prove process replacement and route isolation without
//! embedding any consumer-specific policy.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{
    write_daemon_identity_file, BackendEndpointMux, LegacyClassification, MuxPoll,
};
use running_process::broker::broker_owned_bind;
use running_process::broker::lifecycle::names_v2::{broker_v2_runtime_dir, daemon_identity_path};
use running_process::broker::protocol::{encode_framed, Endpoint, Frame};
use running_process::broker::secure_dir::ensure_private_dir;
use running_process::broker::server::{
    BACKEND_ENV_ENDPOINT_NAMESPACE, BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_SERVICE_NAME,
};
use running_process::client::{IpcEndpoint, IpcListener, IpcStream};

const LIFECYCLE_TEST_PAYLOAD_PROTOCOL: u32 = 0xF824;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("running-process lifecycle test backend failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> io::Result<()> {
    let service_name = required_env(BACKEND_ENV_SERVICE_NAME)?;
    let endpoint_path = required_env(BACKEND_ENV_ENDPOINT_PATH)?;
    let namespace_id = std::env::var(BACKEND_ENV_ENDPOINT_NAMESPACE).unwrap_or_default();
    let endpoint = Endpoint {
        namespace_id,
        path: endpoint_path.clone(),
    };

    let listener = match broker_owned_bind::recover_from_env()? {
        Some(listener) => listener,
        None => IpcListener::bind_owner_only(&IpcEndpoint::new(endpoint_path.clone())?)?,
    };
    let daemon = DaemonProcess::current_process(endpoint, None).map_err(io::Error::other)?;

    let runtime_dir = broker_v2_runtime_dir();
    ensure_private_dir(&runtime_dir)?;
    write_daemon_identity_file(&daemon_identity_path(&service_name), &daemon)?;

    let mux = BackendEndpointMux::new(
        daemon,
        &[LIFECYCLE_TEST_PAYLOAD_PROTOCOL],
        |_buffer: &[u8]| LegacyClassification::NotLegacy,
    );
    loop {
        let mut stream = listener.accept()?;
        if let Err(error) = serve_one(&mut stream, &mux) {
            eprintln!("running-process lifecycle test backend connection failed: {error}");
        }
    }
}

fn required_env(name: &str) -> io::Result<String> {
    std::env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("required environment variable {name} is missing"),
        )
    })
}

fn serve_one<F>(stream: &mut IpcStream, mux: &BackendEndpointMux<F>) -> io::Result<()>
where
    F: Fn(&[u8]) -> LegacyClassification,
{
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match mux.poll(&buffer).map_err(io::Error::other)? {
            MuxPoll::NeedMoreBytes => {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            MuxPoll::ProbeAnswered { reply, .. } => {
                stream.write_all(&reply)?;
                stream.flush()?;
                return Ok(());
            }
            MuxPoll::Payload { frame, .. } => {
                let response =
                    Frame::response_to(&frame, std::process::id().to_be_bytes().to_vec());
                stream.write_all(&encode_framed(&response).map_err(io::Error::other)?)?;
                stream.flush()?;
                return Ok(());
            }
            MuxPoll::Legacy => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "lifecycle fixture has no legacy wire",
                ));
            }
        }
    }
}
