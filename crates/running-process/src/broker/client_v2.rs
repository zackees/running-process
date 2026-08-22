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
use std::time::Duration;

use interprocess::local_socket::Stream as LegacyStream;
use prost::Message;
use running_process_platform_internal::{into_legacy_ipc_stream, platform::ipc};

/// Default deadline for the Hello round-trip in [`connect`].
///
/// Mirrors v1's `AsyncBrokerSession::adopt` budget (~3s). A v2 broker
/// that accepts the dial but stalls (deadlock, GC pause, hung backend
/// resolver, ENOSPC log write) would otherwise hang the caller
/// indefinitely — local-socket streams have no portable read deadline,
/// so the only bound is via a helper thread + `recv_timeout`. Fixes
/// #517.
pub const DEFAULT_HELLO_DEADLINE: Duration = Duration::from_secs(3);

use crate::broker::adopt::{IntoBackendIoError, OwnedBackendIo};
use crate::broker::client::connect_ipc_stream;
use crate::broker::connect_watchdog::{capture_connect_dump, ConnectWatchdog, WATCHDOG_GRACE};
use crate::broker::lifecycle::names::PipePathError;
use crate::broker::lifecycle::names_v2::{
    broker_path_scope_hash, v2_program_pipe, BrokerPathIdentityError,
};
use crate::broker::lifecycle::sid::{user_sid_hash, SidError};
use crate::broker::protocol::{
    hello_reply, read_frame, validate_frame_envelope, write_frame, Frame, FrameKind,
    FrameValidationError, FramingError, Hello, HelloReply, Negotiated, PayloadEncoding, Refused,
    CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION, PROTOCOL_VERSION,
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
    ///
    /// `retry_after_ms` is promoted from `details.retry_after_ms` to a
    /// top-level field so RateLimited callers don't have to thread the
    /// boxed prost payload back out to honor broker-supplied backoff.
    /// Matches the shape of v1's `BrokerClientError::Refused`. Fixes
    /// #518. `details` is kept so any future scalar / nested field in
    /// the prost message stays accessible without another API break.
    #[error("broker refused Hello: {reason}")]
    Refused {
        /// Human-readable refusal text.
        reason: String,
        /// Suggested back-off before retrying (0 = no hint). Mirrors the
        /// proto wire type (`Refused.retry_after_ms` is `uint64`).
        retry_after_ms: u64,
        /// Decoded refused payload for further inspection by callers.
        details: Box<Refused>,
    },

    /// Encoding the outbound `Hello` failed.
    #[error("Hello encode: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// Error from the install-path-scoped broker connection API.
///
/// This is deliberately separate from [`BrokerV2Error`] so adding the new
/// identity failure cannot break downstream exhaustive matches on that
/// established public enum.
#[derive(Debug, thiserror::Error)]
pub enum BrokerPathConnectError {
    /// The installed broker path could not be canonicalized exactly.
    #[error(transparent)]
    Identity(#[from] BrokerPathIdentityError),

    /// The derived endpoint could not complete the v2 broker handshake.
    #[error(transparent)]
    Connect(#[from] BrokerV2Error),
}

/// Internal error vocabulary for the compatibility adapter's explicit-Hello
/// path. Keeping the additional validation stages here preserves exhaustive
/// downstream matches on the established public [`BrokerV2Error`] enum.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ExplicitHelloError {
    /// An error already represented by the stable v2 client API.
    #[error(transparent)]
    Broker(#[from] BrokerV2Error),

    /// The outer response `Frame` failed to decode.
    #[error("response Frame decode: {0}")]
    DecodeFrame(prost::DecodeError),

    /// The broker returned a decodable response with an invalid envelope or
    /// request correlation.
    #[error("unexpected broker response frame: {0}")]
    UnexpectedResponseFrame(&'static str),
}

impl ExplicitHelloError {
    fn into_broker_v2(self) -> BrokerV2Error {
        match self {
            Self::Broker(error) => error,
            Self::DecodeFrame(error) => BrokerV2Error::Decode(error),
            Self::UnexpectedResponseFrame(reason) => {
                BrokerV2Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, reason))
            }
        }
    }
}

/// Async counterpart of [`ClientSession`] for tokio callers.
///
/// Both v2 client operations block: `connect_with_deadline` bounds the Hello
/// with a helper thread and `recv_timeout`, and the backend dial is a blocking
/// `connect`. Neither may run on a runtime worker, so each is wrapped in
/// `spawn_blocking` here — the same approach v1's `AsyncBrokerSession` takes,
/// and for the same reason: the v2 wire is defined against blocking I/O, and
/// duplicating it against `AsyncRead`/`AsyncWrite` would mean two wire
/// implementations to keep in step.
///
/// The pair this exists for is v1's `AsyncBrokerSession::adopt` ->
/// `into_backend_io`, which is what `client_compat` re-exports today (#532
/// criterion 5). Matching that shape is what lets those re-exports point at
/// `client_v2` without the consumer changing.
#[cfg(feature = "client-async")]
#[derive(Debug)]
pub struct AsyncClientSession {
    inner: ClientSession,
}

#[cfg(feature = "client-async")]
impl AsyncClientSession {
    /// Negotiate with the v2 broker on a blocking worker.
    ///
    /// Bounded by [`DEFAULT_HELLO_DEADLINE`]; for a custom bound use
    /// [`connect_with_deadline`](Self::connect_with_deadline).
    pub async fn connect(program: &str, version_hint: &str) -> Result<Self, AsyncConnectError> {
        Self::connect_with_deadline(program, version_hint, DEFAULT_HELLO_DEADLINE).await
    }

    /// [`connect`](Self::connect) with a caller-supplied Hello deadline.
    pub async fn connect_with_deadline(
        program: &str,
        version_hint: &str,
        deadline: Duration,
    ) -> Result<Self, AsyncConnectError> {
        let program = program.to_owned();
        let version_hint = version_hint.to_owned();
        let joined = tokio::task::spawn_blocking(move || {
            super::client_v2::connect_with_deadline(&program, &version_hint, deadline)
        })
        .await
        .map_err(|err| AsyncConnectError::Join(err.to_string()))?;
        Ok(Self { inner: joined? })
    }

    /// The broker's negotiated reply to our `Hello`.
    pub fn negotiated(&self) -> &Negotiated {
        self.inner.negotiated()
    }

    /// Dial the negotiated backend on a blocking worker.
    ///
    /// `async` rather than a plain delegate because the dial is a blocking
    /// `connect` on a local socket: calling it directly from a task would
    /// stall a runtime worker for as long as the backend takes to accept,
    /// which for an unresponsive backend is the whole connect timeout.
    pub async fn connect_backend(self) -> Result<LegacyStream, AsyncConnectError> {
        let inner = self.inner;
        tokio::task::spawn_blocking(move || inner.connect_backend())
            .await
            .map_err(|err| AsyncConnectError::Join(err.to_string()))?
            .map_err(AsyncConnectError::Dial)
    }

    /// [`connect_backend`](Self::connect_backend), handed back as an owned OS
    /// handle. The v2 counterpart of v1's `AsyncBrokerSession::into_backend_io`.
    pub async fn into_backend_io(self) -> Result<OwnedBackendIo, AsyncConnectError> {
        let inner = self.inner;
        tokio::task::spawn_blocking(move || inner.into_backend_io())
            .await
            .map_err(|err| AsyncConnectError::Join(err.to_string()))?
            .map_err(AsyncConnectError::Dial)
    }

    /// Drop to the blocking session.
    pub fn into_blocking(self) -> ClientSession {
        self.inner
    }
}

/// Failure from an [`AsyncClientSession`] operation.
///
/// Keeps the blocking errors intact rather than flattening them: a caller
/// distinguishing a refusal from a dial failure must still be able to, and a
/// runtime-level join failure is neither of those things and should not be
/// disguised as one.
#[cfg(feature = "client-async")]
#[derive(Debug, thiserror::Error)]
pub enum AsyncConnectError {
    /// The broker exchange itself failed.
    #[error(transparent)]
    Broker(#[from] BrokerV2Error),

    /// The negotiated backend could not be dialed.
    #[error(transparent)]
    Dial(#[from] BackendDialError),

    /// The blocking worker did not report back — the task panicked or the
    /// runtime shut down under it. Distinct from both of the above: nothing
    /// was learned about the broker or the backend.
    #[error("the blocking worker did not complete: {0}")]
    Join(String),
}

/// A live session with the v2 broker.
///
/// Wraps the underlying local IPC stream plus the broker's [`Negotiated`]
/// reply. Future slices add operations on top (streaming frames, HTTP
/// endpoint discovery, etc.); slice 4 exposes only the handshake
/// result so downstream consumers can pin the API shape now.
#[derive(Debug)]
pub struct ClientSession {
    stream: ipc::Stream,
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
    pub fn into_inner(self) -> (LegacyStream, Negotiated) {
        (into_legacy_ipc_stream(self.stream), self.negotiated)
    }

    /// Dial the backend the broker named, and hand back that connection.
    ///
    /// The stream inside a [`ClientSession`] is connected to the **broker**,
    /// not to the backend — it exists to carry the Hello. The data connection
    /// is a second dial, to `Negotiated.backend_pipe`, and this performs it.
    ///
    /// This is v1's `BrokerNegotiated` route, step for step: `client.rs`
    /// reads the `HelloReply`, refuses an empty `backend_pipe`, then calls
    /// [`crate::broker::client::connect_local_socket`] on it and treats *that* socket as the
    /// connection. Keeping the sequence identical is the point — a consumer
    /// moving from v1's `client_compat` re-exports to `client_v2` must not be
    /// able to tell, and the way to guarantee that is to do the same thing
    /// rather than something equivalent-looking.
    ///
    /// The broker stream is dropped here, as v1 drops it: its job ended with
    /// the reply.
    pub fn connect_backend(self) -> Result<LegacyStream, BackendDialError> {
        self.connect_backend_ipc().map(into_legacy_ipc_stream)
    }

    /// [`connect_backend`](Self::connect_backend) without the legacy
    /// source-compatibility unwrap.
    ///
    /// `connect_backend` predates the opaque IPC facade and keeps handing back
    /// the transport type until the next major release. In-repo callers use
    /// this instead so the native type never leaves `platform::ipc`.
    pub(crate) fn connect_backend_ipc(self) -> Result<ipc::Stream, BackendDialError> {
        if self.negotiated.backend_pipe.is_empty() {
            return Err(BackendDialError::EmptyBackendPipe);
        }
        connect_ipc_stream(&self.negotiated.backend_pipe).map_err(BackendDialError::Connect)
    }

    /// [`connect_backend`](Self::connect_backend), handed back as an owned OS
    /// handle for a consumer that wants to run its own protocol over it.
    ///
    /// The v2 counterpart of v1's `into_backend_io`. v1 can hand back the
    /// session's own stream because by then it is already the backend
    /// connection; here the dial happens first, so the handle a caller
    /// receives is the same kind of socket either way.
    ///
    /// Unix-only, matching v1: the Windows `OwnedHandle` path is deferred
    /// (#720) and returns `IntoBackendIoError::WindowsUnsupported`. Plain
    /// backticks, not an intra-doc link: that variant is `#[cfg(windows)]`,
    /// so a link to it is unresolvable on the platform CI documents. The
    /// neighbouring reference in `adopt.rs` is written the same way for the
    /// same reason. zccache
    /// already re-dials with its own transport on Windows for that reason, so
    /// this parity is what keeps its two platform lanes unchanged.
    pub fn into_backend_io(self) -> Result<OwnedBackendIo, BackendDialError> {
        let stream = self.connect_backend_ipc()?;
        OwnedBackendIo::from_local_socket_stream(stream).map_err(BackendDialError::IntoBackendIo)
    }
}

/// Why dialing the negotiated backend failed.
///
/// Mirrors the v1 distinctions rather than collapsing them: "the broker named
/// no backend" and "the backend would not accept" call for different consumer
/// behaviour, and v1 already separates them (`EmptyBackendPipe` vs
/// `BackendConnect`).
#[derive(Debug, thiserror::Error)]
pub enum BackendDialError {
    /// The broker negotiated successfully but named no backend.
    ///
    /// Not a refusal: the v2 broker replies with an empty `backend_pipe` when
    /// a service is registered and version-compatible but its daemon has not
    /// published yet. Retrying later can succeed, which is why this is not
    /// folded into the connect error.
    #[error("broker negotiated but named no backend pipe")]
    EmptyBackendPipe,

    /// The backend pipe was named but would not accept a connection.
    #[error("could not connect to the negotiated backend: {0}")]
    Connect(#[source] std::io::Error),

    /// The connection was made but could not be handed back as an owned
    /// handle — on Windows, always (#720).
    #[error("could not take ownership of the backend socket: {0}")]
    IntoBackendIo(#[source] IntoBackendIoError),
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
///
/// Bounded by [`DEFAULT_HELLO_DEADLINE`]; for a custom deadline use
/// [`connect_with_deadline`].
pub fn connect(program: &str, version_hint: &str) -> Result<ClientSession, BrokerV2Error> {
    connect_service(program, program, version_hint)
}

/// Dial the v2 broker bound for `program` while routing the Hello to an
/// independently named `service_name`.
///
/// Shared brokers use one stable bind namespace and select among backend
/// partitions through `Hello.service_name`. [`connect`] remains the convenient
/// same-name form for dedicated brokers.
pub fn connect_service(
    program: &str,
    service_name: &str,
    version_hint: &str,
) -> Result<ClientSession, BrokerV2Error> {
    connect_service_with_deadline(program, service_name, version_hint, DEFAULT_HELLO_DEADLINE)
}

/// Connect to the v2 broker for `program`, or terminate the process.
///
/// The fail-fast entry point for callers that must never spin on an
/// unreachable daemon (running-process#894). Where [`connect`] returns an
/// error the caller might loop on — the retry/respawn behaviour that pinned
/// every core and hung the machine downstream — this makes an unreachable
/// daemon terminal: one bounded attempt, then an all-thread stack dump (so the
/// stuck thread is visible) and `exit 1`. It never retries and never respawns.
///
/// An out-of-band [`ConnectWatchdog`] guarantees termination even if the dump
/// or the exit epilogue itself wedges: the attempt must finish within
/// `deadline + WATCHDOG_GRACE` or the process is aborted. On the success path
/// the watchdog is disarmed as the returned session leaves this function.
///
/// This does not return on failure; the return type reflects the success case.
pub fn connect_or_die(program: &str, version_hint: &str, deadline: Duration) -> ClientSession {
    // Armed for the whole attempt. Dropped (disarmed) only when we return a
    // session below; `std::process::exit` skips destructors, so the terminal
    // path deliberately leaves it armed as a backstop.
    let watchdog = ConnectWatchdog::arm(deadline + WATCHDOG_GRACE);

    match connect_with_deadline(program, version_hint, deadline) {
        Ok(session) => {
            drop(watchdog);
            session
        }
        Err(err) => {
            let error = err.to_string();
            eprintln!(
                "running-process: v2 broker for '{program}' unreachable within \
                 {deadline:?}: {error} — capturing a stack dump and exiting (no retry)"
            );
            if let Some(path) = capture_connect_dump(program, deadline, &error) {
                eprintln!(
                    "running-process: all-thread stack dump written to {}",
                    path.display()
                );
            }
            std::process::exit(1);
        }
    }
}

/// Same as [`connect`] but with a caller-supplied deadline for the
/// Hello round-trip. On deadline returns
/// `BrokerV2Error::Io(ErrorKind::TimedOut)` and the helper thread
/// continues to drain (there is no portable way to cancel a sync
/// `Stream::connect` / framed read mid-call).
///
/// Fixes #517 — without this bound, a v2 broker that accepts the dial
/// then stalls hangs the caller indefinitely.
pub fn connect_with_deadline(
    program: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerV2Error> {
    connect_service_with_deadline(program, program, version_hint, deadline)
}

/// Deadline-bounded form of [`connect_service`].
pub fn connect_service_with_deadline(
    program: &str,
    service_name: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerV2Error> {
    let scope_hash = user_sid_hash()?;
    connect_service_with_scope_hash_and_deadline(
        program,
        &scope_hash,
        service_name,
        version_hint,
        deadline,
    )
}

/// Connect to the broker endpoint derived from an installed broker path.
///
/// The canonical path is the complete scope identity. This intentionally
/// bypasses [`user_sid_hash`]: a machine-wide installation is shared, while a
/// user-private installation already differs by its path.
pub fn connect_service_for_broker_path_with_deadline(
    program: &str,
    broker_path: impl AsRef<std::path::Path>,
    service_name: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerPathConnectError> {
    let scope_hash = broker_path_scope_hash(broker_path)?;
    let pipe_name = v2_program_pipe(program, &scope_hash, 0).map_err(BrokerV2Error::from)?;
    let socket_path =
        crate::broker::server::singleton_bind::resolve_path_scoped_socket_path(&pipe_name)
            .map_err(|err| {
                BrokerV2Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
            })?;
    Ok(connect_service_at_socket_with_deadline(
        program,
        socket_path,
        service_name,
        version_hint,
        deadline,
    )?)
}

/// Deadline-bounded legacy per-user connect using a caller-supplied scope hash.
///
/// The bare name still resolves through the per-user runtime directory. Do not
/// pass [`broker_path_scope_hash`] here: install-path-scoped callers must use
/// [`connect_service_for_broker_path_with_deadline`] so Unix does not add user
/// identity back into the resolved endpoint.
pub fn connect_service_with_scope_hash_and_deadline(
    program: &str,
    scope_hash: &str,
    service_name: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerV2Error> {
    let pipe_name = v2_program_pipe(program, scope_hash, 0)?;
    let socket_path = resolve_socket_path(&pipe_name);
    connect_service_at_socket_with_deadline(
        program,
        socket_path,
        service_name,
        version_hint,
        deadline,
    )
}

/// Connect to an explicit v2 broker endpoint with a caller-supplied Hello.
///
/// This is the compatibility-preserving form: callers that already own the
/// complete Hello contract (client identity, capabilities, keepalive and
/// request identity) can move to the v2 transport without those fields being
/// replaced by `client_v2` defaults.
#[cfg(feature = "client-async")]
pub(crate) fn connect_hello_at_endpoint_with_deadline(
    broker_endpoint: impl Into<String>,
    hello: Hello,
    deadline: Duration,
) -> Result<ClientSession, ExplicitHelloError> {
    let socket_path = broker_endpoint.into();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(connect_unbounded_with_hello(&socket_path, hello));
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(BrokerV2Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("v2 broker Hello did not complete within {deadline:?}"),
        ))
        .into()),
    }
}

fn connect_service_at_socket_with_deadline(
    program: &str,
    socket_path: String,
    service_name: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerV2Error> {
    let program = program.to_owned();
    let service_name = service_name.to_owned();
    let version_hint = version_hint.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(connect_unbounded(
            &program,
            &socket_path,
            &service_name,
            &version_hint,
        ));
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(BrokerV2Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("v2 broker Hello did not complete within {deadline:?}"),
        ))),
    }
}

/// Inner connect without a deadline. Called from inside the helper
/// thread spawned by [`connect_with_deadline`].
fn connect_unbounded(
    program: &str,
    socket_path: &str,
    service_name: &str,
    version_hint: &str,
) -> Result<ClientSession, BrokerV2Error> {
    let mut stream = connect_ipc_stream(socket_path).map_err(|source| BrokerV2Error::Dial {
        socket_path: socket_path.to_string(),
        source,
    })?;
    let hello = default_hello(program, service_name, version_hint);
    let negotiated =
        hello_round_trip(&mut stream, hello).map_err(ExplicitHelloError::into_broker_v2)?;
    Ok(ClientSession { stream, negotiated })
}

#[cfg(feature = "client-async")]
fn connect_unbounded_with_hello(
    socket_path: &str,
    hello: Hello,
) -> Result<ClientSession, ExplicitHelloError> {
    let mut stream = connect_ipc_stream(socket_path).map_err(|source| BrokerV2Error::Dial {
        socket_path: socket_path.to_string(),
        source,
    })?;
    let negotiated = hello_round_trip(&mut stream, hello)?;
    Ok(ClientSession { stream, negotiated })
}

fn default_hello(program: &str, service_name: &str, version_hint: &str) -> Hello {
    Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: service_name.to_string(),
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
    }
}

fn hello_round_trip<S: Read + Write>(
    stream: &mut S,
    hello: Hello,
) -> Result<Negotiated, ExplicitHelloError> {
    // The wire-level `write_frame`/`read_frame` pair is only the raw
    // length-prefixed byte framing (`protocol::framing`) -- v1's actual
    // message framing is the `Frame` protobuf envelope
    // (`envelope_version`/`kind`/`payload`/...), which the server's
    // `connection.rs` accept loop `Frame::decode`s on every Hello and
    // `Frame`-wraps every reply (`write_response_frame`). Sending the bare
    // `Hello` bytes here (as this function previously did) is a genuine
    // client/server framing mismatch: the server's `Frame::decode` of a
    // bare `Hello` payload happens to succeed anyway (both messages start
    // with low-numbered fields), but the reply comes back `Frame`-wrapped,
    // and decoding those bytes directly as `HelloReply` misreads `Frame`'s
    // own fields (e.g. `envelope_version`, a `Varint`) as `HelloReply`'s
    // `result` oneof (which is entirely message-typed, `LengthDelimited`)
    // -- exactly the `UnexpectedWireType { actual: Varint, expected:
    // LengthDelimited }` decode failure this was caught by (soldr#2364).
    let hello_bytes = hello.encode_to_vec();
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: hello_bytes,
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    let body = request_frame.encode_to_vec();
    write_frame(stream, &body).map_err(BrokerV2Error::from)?;

    let reply_frame_bytes = read_frame(stream).map_err(BrokerV2Error::from)?;
    let reply_frame =
        Frame::decode(reply_frame_bytes.as_slice()).map_err(ExplicitHelloError::DecodeFrame)?;
    validate_frame_envelope(&reply_frame, FrameKind::Response, CONTROL_PAYLOAD_PROTOCOL)
        .map_err(map_response_frame_validation)?;
    if reply_frame.request_id != request_frame.request_id {
        return Err(ExplicitHelloError::UnexpectedResponseFrame(
            "request_id does not match the Hello request",
        ));
    }
    let reply =
        HelloReply::decode(reply_frame.payload.as_slice()).map_err(BrokerV2Error::Decode)?;
    match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => Ok(n),
        Some(hello_reply::Result::Refused(r)) => Err(BrokerV2Error::Refused {
            reason: r.reason.clone(),
            retry_after_ms: r.retry_after_ms,
            details: Box::new(r),
        }
        .into()),
        None => Err(BrokerV2Error::MissingResult.into()),
    }
}

fn map_response_frame_validation(error: FrameValidationError) -> ExplicitHelloError {
    ExplicitHelloError::UnexpectedResponseFrame(match error {
        FrameValidationError::EnvelopeVersion { .. } => "envelope_version is not v1",
        FrameValidationError::Kind { .. } => "kind is not RESPONSE",
        FrameValidationError::PayloadProtocol { .. } => "payload_protocol is not control-plane",
        FrameValidationError::PayloadEncoding { .. } => "payload is compressed",
    })
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

/// Resolve a test endpoint the same way the code under test does.
///
/// Deliberately goes through `platform::ipc` rather than re-deriving a name:
/// every bind and dial must share the canonical conversion boundary so a
/// resolved Windows pipe cannot acquire its namespace twice.
#[cfg(test)]
fn test_endpoint(socket_path: &str) -> ipc::Endpoint {
    ipc::Endpoint::new(socket_path.to_owned()).expect("test endpoint")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    struct ScriptedHelloIo {
        response: std::io::Cursor<Vec<u8>>,
        request: Vec<u8>,
    }

    impl Read for ScriptedHelloIo {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.response.read(buf)
        }
    }

    impl Write for ScriptedHelloIo {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.request.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scripted_response(body: &[u8]) -> ScriptedHelloIo {
        let mut response = Vec::new();
        write_frame(&mut response, body).expect("frame scripted response");
        ScriptedHelloIo {
            response: std::io::Cursor::new(response),
            request: Vec::new(),
        }
    }

    fn valid_negotiated_response() -> Frame {
        let reply = HelloReply {
            result: Some(hello_reply::Result::Negotiated(Negotiated {
                backend_pipe: "backend".into(),
                ..Default::default()
            })),
        };
        Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id: 1,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        }
    }

    #[test]
    fn hello_rejects_invalid_response_envelopes_and_correlation() {
        let mut invalid = Vec::new();
        let mut frame = valid_negotiated_response();
        frame.envelope_version += 1;
        invalid.push(frame);
        let mut frame = valid_negotiated_response();
        frame.kind = FrameKind::Event as i32;
        invalid.push(frame);
        let mut frame = valid_negotiated_response();
        frame.payload_protocol += 1;
        invalid.push(frame);
        let mut frame = valid_negotiated_response();
        frame.payload_encoding = PayloadEncoding::Zstd as i32;
        invalid.push(frame);
        let mut frame = valid_negotiated_response();
        frame.request_id = 0;
        invalid.push(frame);

        for frame in invalid {
            let mut io = scripted_response(&frame.encode_to_vec());
            assert!(matches!(
                hello_round_trip(&mut io, default_hello("test", "service", "1")),
                Err(ExplicitHelloError::UnexpectedResponseFrame(_))
            ));
        }
    }

    #[test]
    fn hello_distinguishes_outer_frame_and_inner_reply_decode_errors() {
        let mut bad_frame = scripted_response(&[0xff, 0xff, 0xff]);
        assert!(matches!(
            hello_round_trip(&mut bad_frame, default_hello("test", "service", "1")),
            Err(ExplicitHelloError::DecodeFrame(_))
        ));

        let mut frame = valid_negotiated_response();
        frame.payload = vec![0xff, 0xff, 0xff];
        let mut bad_reply = scripted_response(&frame.encode_to_vec());
        assert!(matches!(
            hello_round_trip(&mut bad_reply, default_hello("test", "service", "1")),
            Err(ExplicitHelloError::Broker(BrokerV2Error::Decode(_)))
        ));
    }

    /// Test-side counterpart of [`connect`]'s Frame-wrapping: reads a
    /// length-prefixed `Frame`-wrapped `Hello` off `stream` and decodes
    /// the inner `Hello`. These in-process stub brokers stand in for the
    /// real `serve_launching_backends` accept loop, so they must speak
    /// the same on-wire shape the real server does (soldr#2364) -- a
    /// stub that reads/writes bare `Hello`/`HelloReply` bytes no longer
    /// matches what `connect` sends/expects.
    fn read_hello_frame(stream: &mut impl Read) -> (Hello, u64) {
        let bytes = read_frame(stream).expect("read Hello frame");
        let frame = Frame::decode(bytes.as_slice()).expect("decode Frame");
        (
            Hello::decode(frame.payload.as_slice()).expect("decode Hello"),
            frame.request_id,
        )
    }

    /// Test-side counterpart of [`connect`]'s Frame-wrapping: encodes
    /// `reply` as a `Frame`-wrapped `HelloReply` and writes it to `stream`.
    fn write_hello_reply_frame(stream: &mut impl Write, request_id: u64, reply: &HelloReply) {
        let reply_frame = Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        };
        write_frame(stream, &reply_frame.encode_to_vec()).expect("write HelloReply frame");
    }

    /// RAII guard: on `Drop`, removes the socket file at `path`. Used by
    /// [`spawn_stub_broker`] so a panic between bind and the final
    /// explicit `remove_file` doesn't leak a stale `.sock` that would
    /// poison the next test run.
    ///
    /// Fixes #519: previously, any panic between `tx.send` and the
    /// explicit `remove_file` left a stale socket. The next test run
    /// either got `EADDRINUSE` on bind or `ECONNREFUSED` on connect to
    /// the dead socket — both masking the real failure.
    #[cfg(unix)]
    struct SocketCleanup(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// In-process stub broker: listens on the given path, accepts ONE
    /// connection, reads a Hello, sends back a `Negotiated` with
    /// `connection_id = 0xC0FFEE`. Returns nothing — the test asserts
    /// against the ClientSession the real client builds.
    fn spawn_stub_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let (hello, request_id) = read_hello_frame(&mut stream);
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
            write_hello_reply_frame(&mut stream, request_id, &reply);
            // RAII guard removes the socket on scope exit; the explicit
            // remove that lived here previously was a no-op leftover.
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
    fn connect_service_dials_broker_program_but_routes_named_service() {
        let program = "client-v2-router";
        let service_name = "soldr-daemon-root-version-hash";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);

        let (ready_tx, ready_rx) = mpsc::channel();
        let (hello_tx, hello_rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            ready_tx.send(()).expect("ready");
            let mut stream = listener.accept().expect("accept");
            let (hello, request_id) = read_hello_frame(&mut stream);
            hello_tx
                .send(hello.service_name.clone())
                .expect("observed service name");
            write_hello_reply_frame(
                &mut stream,
                request_id,
                &HelloReply {
                    result: Some(hello_reply::Result::Negotiated(Negotiated {
                        backend_pipe: "route-endpoint".into(),
                        ..Default::default()
                    })),
                },
            );
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("stub broker listening");
        let session = connect_service(program, service_name, "0.8.0")
            .expect("independent service route connects");
        assert_eq!(session.negotiated().backend_pipe, "route-endpoint");
        assert_eq!(
            hello_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            service_name
        );
    }

    #[test]
    fn connect_with_no_broker_returns_dial_error() {
        let err =
            connect("client-v2-no-broker-ever", "0.0.0").expect_err("no broker => Dial error");
        match err {
            BrokerV2Error::Dial { .. } => {}
            other => panic!("expected Dial, got: {other:?}"),
        }
    }

    /// In-process stub that accepts the dial then sleeps forever — the
    /// pathological case that motivated #517. Without the helper-thread
    /// deadline, the client hangs indefinitely.
    fn spawn_stall_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            let _stream = listener.accept().expect("accept");
            // Stall — never reads the Hello, never replies. The deadline
            // bound on the client side is what releases it.
            thread::sleep(Duration::from_secs(60));
        });
        rx
    }

    /// `connect_with_deadline` returns `TimedOut` when the broker
    /// accepts then stalls. Fixes #517.
    #[test]
    fn connect_with_deadline_fires_on_stalling_broker() {
        let program = "client-v2-stall-deadline";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_stall_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("stall broker listening");
        let start = Instant::now();
        let err = connect_with_deadline(program, "0.0.0", Duration::from_millis(200))
            .expect_err("stall broker => deadline TimedOut");
        let elapsed = start.elapsed();
        match err {
            BrokerV2Error::Io(io) => assert_eq!(io.kind(), std::io::ErrorKind::TimedOut),
            other => panic!("expected Io(TimedOut), got: {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline should fire within budget; took {elapsed:?}"
        );
    }

    /// `BrokerV2Error::Refused` exposes `retry_after_ms` as a top-level
    /// field, mirroring v1's `BrokerClientError::Refused`. Fixes #518.
    /// Constructs a stub broker that replies with Refused, asserts the
    /// retry hint surfaces top-level (not buried in `details`).
    fn spawn_refusing_broker(socket_path: String, retry_after_ms: u64) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let (_hello, request_id) = read_hello_frame(&mut stream);
            let reply = HelloReply {
                result: Some(hello_reply::Result::Refused(Refused {
                    code: 0,
                    reason: "stub refusal".to_string(),
                    retry_after_ms,
                    ..Refused::default()
                })),
            };
            write_hello_reply_frame(&mut stream, request_id, &reply);
        });
        rx
    }

    /// Stress stub: accepts `count` connections in a loop, replying
    /// Negotiated to each. Used by the concurrent-connect stress test
    /// to prove the client side doesn't deadlock or leak handles when
    /// many threads dial simultaneously.
    fn spawn_multi_accept_stub_broker(socket_path: String, count: usize) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            for _ in 0..count {
                let mut stream = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let (_hello, request_id) = read_hello_frame(&mut stream);
                let reply = HelloReply {
                    result: Some(hello_reply::Result::Negotiated(Negotiated {
                        negotiated_protocol: ENVELOPE_VERSION as u32,
                        daemon_version: "stub-multi-1".to_string(),
                        backend_pipe: String::new(),
                        warnings: Vec::new(),
                        server_capabilities: 0,
                        keepalive_interval_secs: 0,
                        handle_passed_token: Vec::new(),
                        connection_id: 0x0FFF_F1EE,
                    })),
                };
                write_hello_reply_frame(&mut stream, request_id, &reply);
            }
        });
        rx
    }

    /// Stress test: 8 concurrent `connect_with_deadline` calls against a
    /// multi-accept stub broker. All must succeed within wall-clock
    /// budget — the helper-thread + `recv_timeout` pattern must scale
    /// to concurrent callers without serializing on a global mutex or
    /// deadlocking on the channel.
    #[test]
    fn concurrent_connects_against_multi_accept_broker() {
        let program = "client-v2-concurrent-multi";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        const N: usize = 8;
        let ready = spawn_multi_accept_stub_broker(socket_path, N);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("multi-accept broker listening");

        let start = Instant::now();
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let p = program.to_string();
                thread::spawn(move || connect_with_deadline(&p, "0.0.0", Duration::from_secs(2)))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let elapsed = start.elapsed();

        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            ok, N,
            "all {N} concurrent connects must succeed; got {ok} ok, full results: {results:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "concurrent connect took {elapsed:?}; expected < 5s"
        );
        for session in results.iter().flatten() {
            assert_eq!(session.negotiated().connection_id, 0x0FFF_F1EE);
            assert_eq!(session.negotiated().daemon_version, "stub-multi-1");
        }
    }

    /// Adversarial stub: accepts, reads Hello, replies with a HelloReply
    /// whose `result` oneof is `None` (proto3 default — easy bug if a
    /// future broker forgets to set the variant). Must surface as
    /// `BrokerV2Error::MissingResult`, not be mis-routed as success.
    fn spawn_missing_result_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let (_hello, request_id) = read_hello_frame(&mut stream);
            let reply = HelloReply { result: None };
            write_hello_reply_frame(&mut stream, request_id, &reply);
        });
        rx
    }

    #[test]
    fn connect_rejects_hello_reply_with_missing_result_oneof() {
        let program = "client-v2-missing-result";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_missing_result_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("missing-result broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected MissingResult, got Ok"),
            }
        };
        assert!(
            matches!(err, BrokerV2Error::MissingResult),
            "expected MissingResult, got: {err:?}"
        );
    }

    /// Adversarial: broker accepts then immediately drops the stream
    /// without reading the Hello or writing a reply. Must surface as
    /// a typed transport error (Framing/Io), never as a successful
    /// session, never hang past the deadline.
    fn spawn_drop_on_accept_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let endpoint = test_endpoint(&socket_path);
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ipc::Listener::bind(&endpoint).expect("bind test listener");
            tx.send(()).expect("send listener-ready signal");
            let stream = listener.accept().expect("accept");
            drop(stream); // immediate close
        });
        rx
    }

    #[test]
    fn connect_returns_err_on_premature_disconnect() {
        let program = "client-v2-prem-disconnect";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_drop_on_accept_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("drop-on-accept broker listening");
        let start = Instant::now();
        let err = loop {
            match connect_with_deadline(program, "0.0.0", Duration::from_millis(500)) {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected transport error, got Ok"),
            }
        };
        // The exact variant depends on whether the write or read hits the
        // disconnect first: Framing(UnexpectedEof), Io(BrokenPipe), or
        // Dial (rare race). All are transport-class — none is a session.
        match err {
            BrokerV2Error::Framing(_) | BrokerV2Error::Io(_) | BrokerV2Error::Dial { .. } => {}
            other => panic!("expected transport variant, got: {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not hang past deadline; took {:?}",
            start.elapsed()
        );
    }

    /// Adversarial: every malformed program name must be rejected BEFORE
    /// `Stream::connect` runs — proves `v2_program_pipe`'s validation is
    /// the front gate. Catches NUL injection, path traversal, uppercase,
    /// over-long names, and empties. The expected error variant is
    /// `BrokerV2Error::PipeName(_)` because `v2_program_pipe`'s
    /// `validate_service_name` fires before any IO.
    #[test]
    fn connect_rejects_invalid_program_names_before_dial() {
        let too_long = "a".repeat(65);
        for bad in [
            "zccache\0evil",
            "../etc/passwd",
            r"a\b",
            "Zccache",
            "a b",
            too_long.as_str(),
            "",
        ] {
            let err = connect(bad, "0.0.0")
                .expect_err(&format!("invalid program name {bad:?} must be rejected"));
            assert!(
                matches!(err, BrokerV2Error::PipeName(_)),
                "expected PipeName for {bad:?}, got: {err:?}"
            );
        }
    }

    /// Pin u64::MAX round-trips through `retry_after_ms` without overflow.
    /// `Duration::from_millis(u64::MAX)` is valid (~584M years); locks
    /// the contract for any caller doing `Duration::from_millis(retry_after_ms)`.
    #[test]
    fn refused_with_u64_max_retry_after_ms_round_trips() {
        let program = "client-v2-refused-u64-max";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_refusing_broker(socket_path, u64::MAX);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("refusing broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected Refused, got Ok"),
            }
        };
        match err {
            BrokerV2Error::Refused {
                retry_after_ms,
                details,
                ..
            } => {
                assert_eq!(retry_after_ms, u64::MAX);
                assert_eq!(details.retry_after_ms, u64::MAX);
                // Caller-side contract: this Duration construction must not panic.
                let _safe_duration = Duration::from_millis(retry_after_ms);
            }
            other => panic!("expected Refused, got: {other:?}"),
        }
    }

    #[test]
    fn refused_exposes_retry_after_ms_top_level() {
        let program = "client-v2-refused-retry";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_refusing_broker(socket_path, 1234);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("refusing broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected Refused"),
            }
        };
        match err {
            BrokerV2Error::Refused {
                retry_after_ms,
                reason,
                details,
            } => {
                assert_eq!(
                    retry_after_ms, 1234,
                    "retry hint must surface top-level (was: {retry_after_ms})"
                );
                assert_eq!(reason, "stub refusal");
                assert_eq!(
                    details.retry_after_ms, 1234,
                    "details payload still carries the field for full diagnostics"
                );
            }
            other => panic!("expected Refused, got: {other:?}"),
        }
    }

    /// The blocking Hello does not occupy the runtime worker.
    ///
    /// This is the property the async type exists for, and nothing else here
    /// tests it — verified by removing `spawn_blocking` and watching every
    /// other async test still pass. Correctness of the result is identical
    /// either way; what differs is whether the runtime can do anything else
    /// meanwhile.
    ///
    /// Uses the stalling broker so the call reliably takes its full deadline.
    /// On a current-thread runtime a spawned task only runs when the current
    /// task yields, so if the Hello ran inline the flag would still be unset
    /// when the assert executes.
    #[cfg(feature = "client-async")]
    #[test]
    fn the_hello_does_not_occupy_the_runtime_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let program = "client-v2-async-nonblocking";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_stall_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("stall broker listening");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            let progressed = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&progressed);
            let other = tokio::spawn(async move {
                flag.store(true, Ordering::SeqCst);
            });

            let _ = AsyncClientSession::connect_with_deadline(
                program,
                "0.0.0",
                Duration::from_millis(200),
            )
            .await;

            assert!(
                progressed.load(Ordering::SeqCst),
                "the runtime made no progress during the Hello — it ran on the worker"
            );
            let _ = other.await;
        });
    }
}

/// Coverage for the backend dial (#532).
///
/// The dial is the step that makes a v2 session reach a backend at all, and
/// it is the step a consumer swapping off v1's `client_compat` re-exports
/// inherits silently — every signature still compiles whether or not the
/// second connection is made correctly.
#[cfg(test)]
mod backend_dial_tests {
    use super::*;

    /// Build a session with a chosen `backend_pipe`.
    ///
    /// The broker connection's contents are irrelevant — `connect_backend`
    /// drops it — but it must be a real opaque stream so the field being
    /// occupied proves the backend dial does not reuse it.
    fn session_with(broker_endpoint: &str, backend_pipe: &str) -> ClientSession {
        let endpoint = ipc::Endpoint::new(broker_endpoint.to_owned()).expect("broker endpoint");
        let stream = ipc::Stream::connect(&endpoint).expect("dial broker");
        ClientSession {
            stream,
            negotiated: Negotiated {
                backend_pipe: backend_pipe.to_string(),
                ..Default::default()
            },
        }
    }

    fn temp_endpoint(tag: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = if cfg!(windows) {
            format!(r"\.\pipe\rp-v2-dial-{tag}-{}", std::process::id())
        } else {
            dir.path().join(format!("{tag}.sock")).display().to_string()
        };
        (dir, path)
    }

    /// A negotiated reply naming no backend is not a connection failure.
    ///
    /// The v2 broker returns an empty `backend_pipe` when a service is
    /// registered and version-compatible but its daemon has not published
    /// yet. Collapsing that into the connect error would tell a caller the
    /// backend refused it, when nothing was ever dialed — and the two call
    /// for different retry behaviour, which is why v1 separates them too.
    #[test]
    fn a_negotiated_reply_with_no_backend_pipe_is_its_own_error() {
        let (_dir, path) = temp_endpoint("empty");
        let listener = ipc::Listener::bind(&test_endpoint(&path)).expect("bind");
        let session = session_with(&path, "");
        let _accepted = listener.accept().expect("accept");

        let err = session
            .connect_backend()
            .expect_err("an empty backend pipe must not be dialed");
        assert!(
            matches!(err, BackendDialError::EmptyBackendPipe),
            "expected EmptyBackendPipe, got {err:?}"
        );
    }

    /// The dial reaches the backend, and the returned socket is live.
    ///
    /// Asserting a byte round-trip rather than just `is_ok()`: a function
    /// that returned the *broker* stream — the mistake this whole change is
    /// about — would also return `Ok`, and would also look connected. Only
    /// traffic arriving at the backend's listener distinguishes them.
    #[test]
    fn the_dial_connects_to_the_backend_and_carries_traffic() {
        let (_bdir, broker_path) = temp_endpoint("broker");
        let broker_listener =
            ipc::Listener::bind(&test_endpoint(&broker_path)).expect("bind broker");
        let (_kdir, backend_path) = temp_endpoint("backend");
        let backend_listener =
            ipc::Listener::bind(&test_endpoint(&backend_path)).expect("bind backend");
        let session = session_with(&broker_path, &backend_path);
        let _broker_accepted = broker_listener.accept().expect("accept broker");

        // Accept on a helper thread with a deadline. A bare `accept()` blocks
        // forever when nothing dials, so a regression that skips the dial
        // would hang this test rather than fail it — and a hang is only
        // caught by nextest's 2-minute killer, which reports a timeout rather
        // than the reason. Verified: with the dial removed, this now fails in
        // seconds saying nothing reached the backend.
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = accepted_tx.send(backend_listener.accept());
        });

        let mut data = session
            .connect_backend()
            .expect("dial the negotiated backend");

        // Arrives at the backend's listener, not the broker's.
        let mut served = accepted_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("nothing connected to the backend within 10s")
            .expect("backend accept");
        data.write_all(b"ping").expect("write to backend");
        data.flush().expect("flush");
        let mut got = [0u8; 4];
        served.read_exact(&mut got).expect("backend read");
        assert_eq!(&got, b"ping", "bytes did not reach the backend");
    }

    /// A named-but-dead backend is a connect error, not a panic.
    #[test]
    fn a_backend_that_is_not_listening_reports_a_connect_error() {
        let (_bdir, broker_path) = temp_endpoint("broker2");
        let broker_listener =
            ipc::Listener::bind(&test_endpoint(&broker_path)).expect("bind broker");
        let (_kdir, dead_path) = temp_endpoint("nobody-home");
        let session = session_with(&broker_path, &dead_path);
        let _broker_accepted = broker_listener.accept().expect("accept broker");
        let err = session
            .connect_backend()
            .expect_err("nothing is listening there");
        assert!(
            matches!(err, BackendDialError::Connect(_)),
            "expected Connect, got {err:?}"
        );
    }

    /// A current-thread runtime is enough: `spawn_blocking` uses the separate
    /// blocking pool, and this crate's tokio is built without `macros` or
    /// `rt-multi-thread`, so `#[tokio::test]` is not available.
    #[cfg(feature = "client-async")]
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime")
    }

    /// The async path dials the backend and the socket it yields is live.
    ///
    /// Same assertion as the blocking test and for the same reason: returning
    /// the broker stream would also be `Ok`. This additionally proves the
    /// `spawn_blocking` hop preserves the connection — a socket that did not
    /// survive being moved across threads would fail here and nowhere else.
    #[cfg(feature = "client-async")]
    #[test]
    fn the_async_dial_reaches_the_backend() {
        let (_bdir, broker_path) = temp_endpoint("abroker");
        let broker_listener =
            ipc::Listener::bind(&test_endpoint(&broker_path)).expect("bind broker");
        let (_kdir, backend_path) = temp_endpoint("abackend");
        let backend_listener =
            ipc::Listener::bind(&test_endpoint(&backend_path)).expect("bind backend");
        let inner = session_with(&broker_path, &backend_path);
        let _broker_accepted = broker_listener.accept().expect("accept broker");

        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = accepted_tx.send(backend_listener.accept());
        });

        let session = AsyncClientSession { inner };
        let mut data = runtime()
            .block_on(session.connect_backend())
            .expect("async dial");

        let mut served = accepted_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("nothing connected to the backend within 10s")
            .expect("backend accept");
        data.write_all(b"pong").expect("write");
        data.flush().expect("flush");
        let mut got = [0u8; 4];
        served.read_exact(&mut got).expect("read");
        assert_eq!(&got, b"pong", "bytes did not reach the backend");
    }

    /// A dial failure stays a dial failure across the runtime hop.
    ///
    /// The hazard the error type exists for: wrapping the blocking call in
    /// `spawn_blocking` introduces a second failure mode (the worker not
    /// reporting back), and it would be easy to collapse both into one
    /// variant. A caller that cannot tell "the backend refused" from "the
    /// runtime went away" cannot decide whether retrying is meaningful.
    #[cfg(feature = "client-async")]
    #[test]
    fn a_dial_failure_is_not_reported_as_a_runtime_failure() {
        let (_bdir, broker_path) = temp_endpoint("abroker2");
        let broker_listener =
            ipc::Listener::bind(&test_endpoint(&broker_path)).expect("bind broker");
        let (_kdir, dead_path) = temp_endpoint("anobody");
        let inner = session_with(&broker_path, &dead_path);
        let _broker_accepted = broker_listener.accept().expect("accept broker");
        let session = AsyncClientSession { inner };
        let err = runtime()
            .block_on(session.connect_backend())
            .expect_err("nothing is listening there");
        assert!(
            matches!(err, AsyncConnectError::Dial(BackendDialError::Connect(_))),
            "expected Dial(Connect), got {err:?}"
        );
    }

    /// An empty backend pipe keeps its identity through the async path too.
    #[cfg(feature = "client-async")]
    #[test]
    fn an_empty_backend_pipe_survives_the_async_hop() {
        let (_dir, path) = temp_endpoint("aempty");
        let listener = ipc::Listener::bind(&test_endpoint(&path)).expect("bind");
        let inner = session_with(&path, "");
        let _accepted = listener.accept().expect("accept");

        let session = AsyncClientSession { inner };
        let err = runtime()
            .block_on(session.connect_backend())
            .expect_err("an empty pipe must not be dialed");
        assert!(
            matches!(
                err,
                AsyncConnectError::Dial(BackendDialError::EmptyBackendPipe)
            ),
            "expected Dial(EmptyBackendPipe), got {err:?}"
        );
    }
}
