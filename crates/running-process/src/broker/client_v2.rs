//! v2 broker client (slice 4 of #488).
//!
//! Counterpart of [`super::client`]. Single public entry point
//! [`connect`]: dial the v2 broker pipe by program name, exchange a
//! Hello / Negotiated, return a [`ClientSession`] handle.
//!
//! The v2 broker fronts each program via the namespace defined by
//! [`super::lifecycle::names_v2::v2_program_pipe`]. The Hello round-trip
//! itself reuses v1's framing (`protocol::{read_frame, write_frame}`)
//! and message shapes (`Hello`, `HelloReply`) per #470's coexistence
//! table. Subsequent slices add post-Hello operations (streaming,
//! HTTP endpoint discovery, etc.); this slice exposes only the
//! handshake so downstream consumers (zccache et al.) can pin against
//! a stable v2 client API while the broker side grows under them.

use std::io::{Read, Write};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;
use prost::Message;

use crate::broker::lifecycle::names::PipePathError;
use crate::broker::lifecycle::names_v2::v2_program_pipe;
use crate::broker::lifecycle::sid::{user_sid_hash, SidError};
use crate::broker::protocol::{
    hello_reply, read_frame, write_frame, FramingError, Hello, HelloReply, Negotiated, Refused,
    ENVELOPE_VERSION,
};

/// Errors surfaced by [`connect`].
#[derive(Debug, thiserror::Error)]
pub enum BrokerV2Error {
    /// `user_sid_hash` failed.
    #[error(transparent)]
    Sid(#[from] SidError),

    /// Building the v2 pipe name failed.
    #[error(transparent)]
    PipeName(#[from] PipePathError),

    /// Dialing the v2 broker pipe failed (no listener, permission denied, ...).
    #[error("dial v2 broker pipe at {socket_path:?}: {source}")]
    Dial {
        /// Path the client attempted to dial.
        socket_path: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Framing-layer error on read or write (envelope version mismatch,
    /// truncated body, oversized frame, ...).
    #[error(transparent)]
    Framing(#[from] FramingError),

    /// Underlying IO failure during Hello / HelloReply exchange.
    #[error("Hello round-trip io: {0}")]
    Io(#[from] std::io::Error),

    /// `HelloReply` payload failed to decode.
    #[error("HelloReply decode: {0}")]
    Decode(#[from] prost::DecodeError),

    /// `HelloReply` was syntactically valid but missing its `result` oneof.
    #[error("HelloReply.result missing")]
    MissingResult,

    /// Broker explicitly refused the Hello (returned a `Refused` reply).
    #[error("broker refused Hello: {reason}")]
    Refused {
        /// Human-readable refusal text.
        reason: String,
        /// Decoded refused payload for further inspection by callers.
        details: Box<Refused>,
    },

    /// Encoding the outbound `Hello` failed.
    #[error("Hello encode: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// A live session with the v2 broker.
///
/// Wraps the underlying [`Stream`] plus the broker's [`Negotiated`]
/// reply. Future slices add operations on top (streaming frames, HTTP
/// endpoint discovery, etc.); slice 4 exposes only the handshake
/// result so downstream consumers can pin the API shape now.
#[derive(Debug)]
pub struct ClientSession {
    stream: Stream,
    negotiated: Negotiated,
}

impl ClientSession {
    /// The broker's negotiated reply to our `Hello`.
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// Consume the session into the raw byte stream + negotiated reply.
    ///
    /// Slices that add post-handshake operations build them on this
    /// raw stream until the v2 client surface stabilizes.
    pub fn into_inner(self) -> (Stream, Negotiated) {
        (self.stream, self.negotiated)
    }
}

/// Dial the v2 broker for `program` and exchange Hello / Negotiated.
///
/// Computes the pipe name via [`v2_program_pipe`], dials it, sends a
/// Hello carrying `program` as `service_name` and `version_hint` as
/// `wanted_version`, reads the HelloReply, and either returns a
/// [`ClientSession`] (on `Negotiated`) or a [`BrokerV2Error::Refused`]
/// (on `Refused`).
///
/// `connection_id` on the outbound Hello is left at 0 — the broker
/// assigns one and echoes it in the Negotiated reply.
pub fn connect(program: &str, version_hint: &str) -> Result<ClientSession, BrokerV2Error> {
    let sid = user_sid_hash()?;
    let pipe_name = v2_program_pipe(program, &sid, 0)?;
    let socket_path = resolve_socket_path(&pipe_name);
    let name = wrap_socket_name(&socket_path).map_err(|err| BrokerV2Error::Dial {
        socket_path: socket_path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
    })?;
    let mut stream = Stream::connect(name).map_err(|source| BrokerV2Error::Dial {
        socket_path: socket_path.clone(),
        source,
    })?;
    let negotiated = hello_round_trip(&mut stream, program, version_hint)?;
    Ok(ClientSession { stream, negotiated })
}

fn hello_round_trip<S: Read + Write>(
    stream: &mut S,
    program: &str,
    version_hint: &str,
) -> Result<Negotiated, BrokerV2Error> {
    let hello = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.to_string(),
        wanted_version: version_hint.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: format!("client_v2-{program}-{}", std::process::id()),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process broker::client_v2".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    };
    let mut body = Vec::with_capacity(hello.encoded_len());
    hello.encode(&mut body)?;
    write_frame(stream, &body)?;

    let reply_bytes = read_frame(stream)?;
    let reply = HelloReply::decode(reply_bytes.as_slice())?;
    match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => Ok(n),
        Some(hello_reply::Result::Refused(r)) => Err(BrokerV2Error::Refused {
            reason: r.reason.clone(),
            details: Box::new(r),
        }),
        None => Err(BrokerV2Error::MissingResult),
    }
}

fn resolve_socket_path(bare_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{bare_name}")
    }
    #[cfg(unix)]
    {
        use std::path::PathBuf;
        let dir: PathBuf = {
            #[cfg(target_os = "macos")]
            {
                let uid = unsafe { libc::getuid() };
                let tmp = std::env::var_os("TMPDIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp"));
                tmp.join(format!(".rp-{uid}-broker-v2"))
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
                    PathBuf::from(d).join("running-process").join("broker-v2")
                } else {
                    let uid = unsafe { libc::getuid() };
                    PathBuf::from(format!("/tmp/running-process-{uid}/broker-v2"))
                }
            }
        };
        let leaf = if cfg!(target_os = "macos") {
            let mut hash = blake3::Hasher::new();
            hash.update(bare_name.as_bytes());
            let bytes = hash.finalize();
            let mut hex = String::with_capacity(16);
            for b in bytes.as_bytes().iter().take(8) {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            format!("{hex}.sock")
        } else {
            format!("{bare_name}.sock")
        };
        dir.join(leaf).to_string_lossy().into_owned()
    }
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use interprocess::local_socket::ListenerOptions;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// In-process stub broker: listens on the given path, accepts ONE
    /// connection, reads a Hello, sends back a `Negotiated` with
    /// `connection_id = 0xC0FFEE`. Returns nothing — the test asserts
    /// against the ClientSession the real client builds.
    fn spawn_stub_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            {
                let _ = std::fs::create_dir_all(
                    std::path::Path::new(&socket_path).parent().unwrap(),
                );
                let _ = std::fs::remove_file(&socket_path);
            }
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let bytes = read_frame(&mut stream).expect("read Hello frame");
            let hello = Hello::decode(bytes.as_slice()).expect("decode Hello");
            let reply = HelloReply {
                result: Some(hello_reply::Result::Negotiated(Negotiated {
                    negotiated_protocol: ENVELOPE_VERSION as u32,
                    daemon_version: "stub-1.2.3".to_string(),
                    backend_pipe: String::new(),
                    warnings: Vec::new(),
                    server_capabilities: 0,
                    keepalive_interval_secs: 0,
                    handle_passed_token: Vec::new(),
                    connection_id: 0x00C0_FFEE,
                })),
            };
            let mut body = Vec::with_capacity(reply.encoded_len());
            reply.encode(&mut body).expect("encode HelloReply");
            write_frame(&mut stream, &body).expect("write HelloReply frame");
            #[cfg(unix)]
            {
                let _ = std::fs::remove_file(&socket_path);
            }
            let _ = hello.service_name;
        });
        rx
    }

    #[test]
    fn connect_completes_hello_round_trip_against_stub_broker() {
        // Use a per-test program name so parallel tests don't collide.
        let program = "client-v2-stub";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);

        let ready = spawn_stub_broker(socket_path.clone());
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("stub broker listening");

        // The Listener on Windows is fully ready as soon as `create_sync`
        // returns; on Unix the same holds. But a short retry loop is
        // resilient to spawning race in CI.
        let start = Instant::now();
        let session = loop {
            match connect(program, "0.0.0") {
                Ok(s) => break s,
                Err(err) if start.elapsed() < Duration::from_secs(2) => {
                    eprintln!("connect retry after error: {err}");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(err) => panic!("connect failed after retries: {err}"),
            }
        };

        let neg = session.negotiated();
        assert_eq!(neg.negotiated_protocol, ENVELOPE_VERSION as u32);
        assert_eq!(neg.connection_id, 0x00C0_FFEE);
        assert_eq!(neg.daemon_version, "stub-1.2.3");
    }

    #[test]
    fn connect_with_no_broker_returns_dial_error() {
        let err = connect("client-v2-no-broker-ever", "0.0.0")
            .expect_err("no broker => Dial error");
        match err {
            BrokerV2Error::Dial { .. } => {}
            other => panic!("expected Dial, got: {other:?}"),
        }
    }
}
