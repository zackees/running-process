//! Supervised direct-endpoint services with a non-blocking client.
//!
//! [`FrameClient`](super::FrameClient) and [`BackendEndpointMux`] are the
//! parts of a direct daemon integration (`docs/INTEGRATE.md`). This module
//! assembles them into the whole thing a small service and its callers
//! need, without the broker:
//!
//! - [`ServiceServer`] binds the service's per-user endpoint exactly once
//!   (a second instance is refused with [`ServiceError::AlreadyRunning`]),
//!   writes the identity sidecar, answers identity probes, and queues
//!   framed requests for the caller's own worker threads.
//! - [`PollingFrameClient`] is the client for callers that must never
//!   block -- an event loop that only has timers, say. Every method returns
//!   immediately. A supervisor thread owns the connection: it finds the
//!   service through its identity file, health-probes it, and when it is
//!   absent or fails a probe, spawns the configured command under a file
//!   lock, within a spawn budget. [`PollingFrameClient::send`] refuses at
//!   once when the service is not up or the queue is full;
//!   [`PollingFrameClient::try_recv`] hands back replies matched to their
//!   request ids.
//!
//! The wire is the frozen v1 frame lane on the service's own payload
//! protocol; nothing here is new on the wire.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::broker::backend_handle::{BackendHandle, Connection, DaemonProcess};
use crate::broker::backend_lifecycle::identity::DaemonIdentityHashPolicy;
use crate::broker::backend_sdk::{
    read_daemon_identity_file, remove_daemon_identity_file, write_daemon_identity_file,
    BackendEndpointMux, LegacyClassification, MuxPoll,
};
use crate::broker::protocol::{
    encode_framed, registry, try_decode_framed, Endpoint, Frame, FrameKind,
};
use crate::broker::server::singleton_bind::{
    bind_singleton, resolve_socket_path, BindSingletonError,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const READ_POLL: Duration = Duration::from_millis(100);
const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(1);
const REPLY_BUFFER_FACTOR: usize = 4;

/// Errors from [`ServiceSpec`] validation and [`ServiceServer`] startup.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The service name is not `[a-z0-9-]{1,64}`.
    #[error("service name {0:?} must match [a-z0-9-]{{1,64}}")]
    InvalidName(String),
    /// The payload protocol is first-party or outside the consumer and
    /// private-use ranges.
    #[error(
        "payload protocol {0:#06X} must be in the registered-consumer range \
         (0x7000..=0x7EFF) or private use (0xF000..=0xFFFF)"
    )]
    InvalidPayloadProtocol(u32),
    /// Another live process already serves this service's endpoint.
    #[error("service {name:?} is already running at {endpoint}")]
    AlreadyRunning {
        /// The service name.
        name: String,
        /// The endpoint that is already bound.
        endpoint: String,
    },
    /// Endpoint resolution, bind, or identity persistence failed.
    #[error("service {name:?}: {source}")]
    Io {
        /// The service name.
        name: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// Everything a service and its clients agree on.
///
/// The endpoint is derived from `name` (and `runtime_dir`, when set) so
/// that two processes built from the same spec meet at the same place:
/// a socket under the per-user runtime directory on Unix, a per-user named
/// pipe on Windows. The identity sidecar and the spawn lock live in
/// [`Self::runtime_dir`].
#[derive(Clone, Debug)]
pub struct ServiceSpec {
    name: String,
    version: String,
    payload_protocol: u32,
    command: Option<Vec<OsString>>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    health_interval: Duration,
    spawn_budget: (u32, Duration),
    spawn_ready_timeout: Duration,
}

impl ServiceSpec {
    /// Validate `name` and `payload_protocol` and build a spec with the
    /// default policy: no spawn command, 5 s health interval, and a spawn
    /// budget of 3 attempts per 30 s (the broker's own default).
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        payload_protocol: u32,
    ) -> Result<Self, ServiceError> {
        let name = name.into();
        let valid_name = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid_name {
            return Err(ServiceError::InvalidName(name));
        }
        let valid_protocol = !registry::is_first_party(payload_protocol)
            && (registry::is_registered_consumer_id(payload_protocol)
                || registry::is_private_use_id(payload_protocol));
        if !valid_protocol {
            return Err(ServiceError::InvalidPayloadProtocol(payload_protocol));
        }
        Ok(Self {
            name,
            version: version.into(),
            payload_protocol,
            command: None,
            env: Vec::new(),
            cwd: None,
            runtime_dir: None,
            health_interval: Duration::from_secs(5),
            spawn_budget: (3, Duration::from_secs(30)),
            spawn_ready_timeout: Duration::from_secs(10),
        })
    }

    /// The command a client runs (detached) when the service is absent.
    /// `None` means clients only connect and never spawn.
    pub fn command<I, S>(mut self, argv: Option<I>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command = argv.map(|argv| argv.into_iter().map(Into::into).collect());
        self
    }

    /// Extra environment for the spawned command.
    pub fn env<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Working directory for the spawned command.
    pub fn cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Override the directory holding the identity sidecar and spawn lock
    /// (and salt the endpoint name with it, so tests can run side by side).
    pub fn runtime_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.runtime_dir = dir;
        self
    }

    /// How often a connected client probes the service's identity.
    pub fn health_interval(mut self, interval: Duration) -> Self {
        self.health_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// At most `attempts` spawns per `window`, per client.
    pub fn spawn_budget(mut self, attempts: u32, window: Duration) -> Self {
        self.spawn_budget = (attempts, window);
        self
    }

    /// How long a spawning client waits for the new instance to answer.
    pub fn spawn_ready_timeout(mut self, timeout: Duration) -> Self {
        self.spawn_ready_timeout = timeout;
        self
    }

    /// The service name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The service version reported in probes.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The frame lane's payload protocol.
    pub fn payload_protocol(&self) -> u32 {
        self.payload_protocol
    }

    /// The directory holding the identity sidecar and the spawn lock.
    pub fn state_dir(&self) -> PathBuf {
        match &self.runtime_dir {
            Some(dir) => dir.clone(),
            None => dirs::runtime_dir()
                .or_else(dirs::data_local_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("running-process")
                .join("services"),
        }
    }

    /// Path of the identity sidecar the server writes and clients probe.
    pub fn identity_path(&self) -> PathBuf {
        self.state_dir()
            .join(format!("{}.identity.json", self.name))
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir().join(format!("{}.spawn.lock", self.name))
    }

    /// The endpoint the service binds and clients dial.
    pub fn endpoint(&self) -> Result<Endpoint, ServiceError> {
        let salt = match &self.runtime_dir {
            Some(dir) => format!("{:08x}", fnv1a32(dir.to_string_lossy().as_bytes())),
            None => "default".to_string(),
        };
        let bare = format!("rp-svc-{}-{}", self.name, salt);
        let path = resolve_socket_path(&bare).map_err(|message| ServiceError::Io {
            name: self.name.clone(),
            source: io::Error::other(message),
        })?;
        Ok(Endpoint {
            namespace_id: self.name.clone(),
            path,
        })
    }
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

type SharedWriter = Arc<Mutex<crate::platform::ipc::Stream>>;

/// One framed request delivered by [`ServiceServer::recv`].
pub struct ServiceRequest {
    frame: Frame,
    writer: SharedWriter,
}

impl ServiceRequest {
    /// The id the client assigned; the reply carries it back.
    pub fn request_id(&self) -> u64 {
        self.frame.request_id
    }

    /// The request payload.
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload
    }

    /// Send the reply. An error means the client has gone away.
    pub fn reply(self, payload: Vec<u8>) -> io::Result<()> {
        let mut template = self.frame;
        template.payload = Vec::new();
        let response = Frame::response_to(&template, payload);
        let wire = encode_framed(&response).map_err(io::Error::other)?;
        let mut stream = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("connection writer poisoned"))?;
        stream.write_all(&wire)?;
        stream.flush()
    }
}

/// A bound, single-instance service endpoint.
///
/// Dropping it (or [`Self::close`]) stops accepting, removes the identity
/// sidecar, and releases the endpoint.
pub struct ServiceServer {
    spec: ServiceSpec,
    endpoint: Endpoint,
    requests: Mutex<Receiver<ServiceRequest>>,
    stop: Arc<AtomicBool>,
    accept: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ServiceServer {
    /// Bind the endpoint, write the identity sidecar, and start accepting.
    pub fn bind(spec: ServiceSpec) -> Result<Self, ServiceError> {
        let io_err = |source: io::Error| ServiceError::Io {
            name: spec.name.clone(),
            source,
        };
        let endpoint = spec.endpoint()?;
        let listener = match bind_singleton(&endpoint.path) {
            Ok(listener) => listener,
            Err(BindSingletonError::AlreadyBound(_)) => {
                return Err(ServiceError::AlreadyRunning {
                    name: spec.name.clone(),
                    endpoint: endpoint.path.clone(),
                })
            }
            Err(BindSingletonError::InvalidName(message)) => {
                return Err(io_err(io::Error::other(message)))
            }
            Err(BindSingletonError::Other(error)) => return Err(io_err(error)),
        };
        let daemon = DaemonProcess::current_process_with_hash_policy(
            endpoint.clone(),
            None,
            DaemonIdentityHashPolicy::Blake3Only,
        )
        .map_err(|error| io_err(io::Error::other(error)))?;
        let identity_path = spec.identity_path();
        if let Some(parent) = identity_path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        write_daemon_identity_file(&identity_path, &daemon).map_err(io_err)?;

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mux = Arc::new(BackendEndpointMux::new(
            daemon,
            &[spec.payload_protocol],
            not_legacy as fn(&[u8]) -> LegacyClassification,
        ));
        let accept_stop = Arc::clone(&stop);
        let accept = thread::Builder::new()
            .name(format!("rp-svc-accept-{}", spec.name))
            .spawn(move || {
                while !accept_stop.load(Ordering::Acquire) {
                    let Ok(stream) = listener.accept() else {
                        continue;
                    };
                    if accept_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mux = Arc::clone(&mux);
                    let tx = tx.clone();
                    let _ = thread::Builder::new()
                        .name("rp-svc-conn".to_string())
                        .spawn(move || serve_connection(stream, &mux, &tx));
                }
            })
            .map_err(io_err)?;
        Ok(Self {
            spec,
            endpoint,
            requests: Mutex::new(rx),
            stop,
            accept: Mutex::new(Some(accept)),
        })
    }

    /// The spec this server was bound with.
    pub fn spec(&self) -> &ServiceSpec {
        &self.spec
    }

    /// Wait up to `timeout` (forever when `None`) for the next request.
    /// Safe to call from several worker threads at once.
    pub fn recv(&self, timeout: Option<Duration>) -> Option<ServiceRequest> {
        // Workers take turns holding the receiver; one waiting for the lock
        // must still honour its own timeout, so poll the lock rather than
        // queueing on it.
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if self.stop.load(Ordering::Acquire) {
                return None;
            }
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            match self.requests.try_lock() {
                Ok(rx) => {
                    let slice = remaining.map_or(READ_POLL, |r| r.min(READ_POLL));
                    match rx.recv_timeout(slice) {
                        Ok(request) => return Some(request),
                        Err(RecvTimeoutError::Disconnected) => return None,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
                Err(std::sync::TryLockError::Poisoned(_)) => return None,
                Err(std::sync::TryLockError::WouldBlock) => {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            if remaining.is_some_and(|r| r.is_zero()) {
                return None;
            }
        }
    }

    /// Stop accepting, remove the identity sidecar, release the endpoint.
    pub fn close(&self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        remove_daemon_identity_file(&self.spec.identity_path());
        // Unblock accept() with a throwaway connection.
        let _ = Connection::connect(&self.endpoint);
        let accept = self.accept.lock().ok().and_then(|mut slot| slot.take());
        if let Some(accept) = accept {
            let _ = accept.join();
        }
    }
}

impl Drop for ServiceServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn not_legacy(_buf: &[u8]) -> LegacyClassification {
    LegacyClassification::NotLegacy
}

fn serve_connection(
    stream: crate::platform::ipc::Stream,
    mux: &BackendEndpointMux<fn(&[u8]) -> LegacyClassification>,
    requests: &Sender<ServiceRequest>,
) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let mut reader = stream;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match mux.poll(&buf) {
            Ok(MuxPoll::NeedMoreBytes) => match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            },
            Ok(MuxPoll::ProbeAnswered { reply, consumed }) => {
                buf.drain(..consumed);
                let Ok(mut w) = writer.lock() else { return };
                if w.write_all(&reply).and_then(|()| w.flush()).is_err() {
                    return;
                }
            }
            Ok(MuxPoll::Payload { frame, consumed }) => {
                buf.drain(..consumed);
                if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
                    continue;
                }
                let request = ServiceRequest {
                    frame,
                    writer: Arc::clone(&writer),
                };
                if requests.send(request).is_err() {
                    return;
                }
            }
            Ok(MuxPoll::Legacy) | Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Non-blocking client
// ---------------------------------------------------------------------------

/// Connection state reported by [`PollingFrameClient::state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// No connection attempt has finished yet.
    Starting,
    /// Connected and passing health probes; `send` accepts work.
    Up,
    /// Not connected; the supervisor is retrying (and spawning, if allowed).
    Down,
}

const STATE_STARTING: u8 = 0;
const STATE_UP: u8 = 1;
const STATE_DOWN: u8 = 2;

/// Why [`PollingFrameClient::send`] refused a request. Both are immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The service is not connected (starting, down, or the client closed).
    #[error("service unavailable")]
    Unavailable,
    /// The outgoing queue is full; the service is not keeping up.
    #[error("service queue full")]
    QueueFull,
}

/// A reply collected by [`PollingFrameClient::try_recv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceReply {
    /// The id [`PollingFrameClient::send`] returned for the request.
    pub request_id: u64,
    /// The service's payload, or why no reply will come (the connection
    /// dropped with the request in flight).
    pub result: Result<Vec<u8>, String>,
}

struct Shared {
    state: AtomicU8,
    stop: AtomicBool,
    replies: Mutex<VecDeque<ServiceReply>>,
    reply_cap: usize,
    in_flight: Mutex<HashSet<u64>>,
}

impl Shared {
    fn push_reply(&self, reply: ServiceReply) {
        if let Ok(mut replies) = self.replies.lock() {
            if replies.len() >= self.reply_cap {
                replies.pop_front();
            }
            replies.push_back(reply);
        }
    }

    fn fail_in_flight(&self, why: &str) {
        let ids: Vec<u64> = match self.in_flight.lock() {
            Ok(mut set) => set.drain().collect(),
            Err(_) => return,
        };
        for request_id in ids {
            self.push_reply(ServiceReply {
                request_id,
                result: Err(why.to_string()),
            });
        }
    }
}

struct Outgoing {
    request_id: u64,
    payload: Vec<u8>,
}

/// Request/reply client whose every method returns immediately.
///
/// Built for callers that cannot block at all -- a UI thread whose event
/// loop offers only timers. Poll [`Self::try_recv`] from a timer; call
/// [`Self::send`] freely, and treat its errors as "drop this request".
pub struct PollingFrameClient {
    shared: Arc<Shared>,
    tx: Mutex<Option<SyncSender<Outgoing>>>,
    next_id: AtomicU64,
    supervisor: Mutex<Option<thread::JoinHandle<()>>>,
}

impl PollingFrameClient {
    /// Start the supervisor thread and return at once. The client is
    /// [`ServiceState::Starting`] until the first connection attempt ends.
    /// `queue_depth` bounds requests waiting to be written.
    pub fn connect(spec: ServiceSpec, queue_depth: usize) -> io::Result<Self> {
        let queue_depth = queue_depth.max(1);
        let shared = Arc::new(Shared {
            state: AtomicU8::new(STATE_STARTING),
            stop: AtomicBool::new(false),
            replies: Mutex::new(VecDeque::new()),
            reply_cap: queue_depth.saturating_mul(REPLY_BUFFER_FACTOR).max(64),
            in_flight: Mutex::new(HashSet::new()),
        });
        let (tx, rx) = mpsc::sync_channel(queue_depth);
        let supervisor_shared = Arc::clone(&shared);
        let supervisor = thread::Builder::new()
            .name(format!("rp-svc-client-{}", spec.name))
            .spawn(move || Supervisor::new(spec, supervisor_shared, rx).run())?;
        Ok(Self {
            shared,
            tx: Mutex::new(Some(tx)),
            next_id: AtomicU64::new(1),
            supervisor: Mutex::new(Some(supervisor)),
        })
    }

    /// Current connection state.
    pub fn state(&self) -> ServiceState {
        match self.shared.state.load(Ordering::Acquire) {
            STATE_UP => ServiceState::Up,
            STATE_DOWN => ServiceState::Down,
            _ => ServiceState::Starting,
        }
    }

    /// Queue one request and return its id. Never waits: refuses with
    /// [`SendError::Unavailable`] unless the service is up, and with
    /// [`SendError::QueueFull`] when `queue_depth` requests are waiting.
    pub fn send(&self, payload: Vec<u8>) -> Result<u64, SendError> {
        if self.state() != ServiceState::Up {
            return Err(SendError::Unavailable);
        }
        let guard = self.tx.lock().map_err(|_| SendError::Unavailable)?;
        let Some(tx) = guard.as_ref() else {
            return Err(SendError::Unavailable);
        };
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match tx.try_send(Outgoing {
            request_id,
            payload,
        }) {
            Ok(()) => Ok(request_id),
            Err(TrySendError::Full(_)) => Err(SendError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(SendError::Unavailable),
        }
    }

    /// Take the oldest collected reply, if any. Never waits.
    pub fn try_recv(&self) -> Option<ServiceReply> {
        self.shared.replies.lock().ok()?.pop_front()
    }

    /// Stop the supervisor and drop the connection. Returns without
    /// waiting for the supervisor thread to finish an in-progress probe.
    pub fn close(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.state.store(STATE_DOWN, Ordering::Release);
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        if let Ok(mut supervisor) = self.supervisor.lock() {
            // Detach: the thread notices `stop` within one poll interval.
            supervisor.take();
        }
    }
}

impl Drop for PollingFrameClient {
    fn drop(&mut self) {
        self.close();
    }
}

struct Supervisor {
    spec: ServiceSpec,
    shared: Arc<Shared>,
    rx: Receiver<Outgoing>,
    spawn_times: VecDeque<Instant>,
    children: Vec<crate::spawn::DaemonChild>,
}

impl Supervisor {
    fn new(spec: ServiceSpec, shared: Arc<Shared>, rx: Receiver<Outgoing>) -> Self {
        Self {
            spec,
            shared,
            rx,
            spawn_times: VecDeque::new(),
            children: Vec::new(),
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn run(mut self) {
        let mut backoff = RETRY_MIN;
        while !self.stopping() {
            self.reap_children();
            match self.establish() {
                Some((stream, identity)) => {
                    backoff = RETRY_MIN;
                    self.run_connection(stream, &identity);
                    self.mark_down("service connection lost");
                }
                None => {
                    self.mark_down("service unavailable");
                    if self.maybe_spawn() {
                        continue;
                    }
                    self.sleep(backoff);
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
        self.mark_down("client closed");
    }

    fn mark_down(&self, why: &str) {
        self.shared.state.store(STATE_DOWN, Ordering::Release);
        // Requests queued while we were up will never be written now.
        while let Ok(out) = self.rx.try_recv() {
            self.shared.push_reply(ServiceReply {
                request_id: out.request_id,
                result: Err(why.to_string()),
            });
        }
        self.shared.fail_in_flight(why);
    }

    fn sleep(&self, total: Duration) {
        let deadline = Instant::now() + total;
        while !self.stopping() {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }

    fn reap_children(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    /// Probe the instance the identity file names; connect if it answers.
    fn probe(&self) -> Option<DaemonProcess> {
        let identity = read_daemon_identity_file(&self.spec.identity_path())?;
        let endpoint = self.spec.endpoint().ok()?;
        if identity.ipc_endpoint.path != endpoint.path {
            return None;
        }
        BackendHandle::probe_with_service_and_timeout(
            self.spec.name.clone(),
            self.spec.version.clone(),
            &identity.ipc_endpoint,
            &identity,
            PROBE_TIMEOUT,
        )
        .ok()?;
        Some(identity)
    }

    fn establish(&self) -> Option<(crate::platform::ipc::Stream, DaemonProcess)> {
        let identity = self.probe()?;
        let stream = Connection::connect(&identity.ipc_endpoint)
            .ok()?
            .into_inner();
        Some((stream, identity))
    }

    /// Spawn the service if allowed. Returns true when it is now answering.
    fn maybe_spawn(&mut self) -> bool {
        let Some(argv) = self.spec.command.clone() else {
            return false;
        };
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        let (attempts, window) = self.spec.spawn_budget;
        let now = Instant::now();
        while self
            .spawn_times
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            self.spawn_times.pop_front();
        }
        if self.spawn_times.len() >= attempts as usize {
            return false;
        }
        let Some(_lock) = SpawnLock::try_acquire(&self.spec.lock_path()) else {
            // Another client is spawning right now; give it time.
            return false;
        };
        if self.probe().is_some() {
            return true;
        }
        self.spawn_times.push_back(now);
        let mut command = std::process::Command::new(program);
        command.args(args);
        for (key, value) in &self.spec.env {
            command.env(key, value);
        }
        if let Some(cwd) = &self.spec.cwd {
            command.current_dir(cwd);
        }
        match crate::spawn::spawn_daemon(&mut command) {
            Ok(child) => self.children.push(child),
            Err(_) => return false,
        }
        let deadline = Instant::now() + self.spec.spawn_ready_timeout;
        while Instant::now() < deadline && !self.stopping() {
            if self.probe().is_some() {
                return true;
            }
            self.reap_children();
            if self.children.is_empty() {
                // The new instance already exited without answering.
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn run_connection(&mut self, stream: crate::platform::ipc::Stream, identity: &DaemonProcess) {
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let _ = reader.set_recv_timeout(Some(READ_POLL));
        let mut writer = stream;
        let dead = Arc::new(AtomicBool::new(false));
        let reader_dead = Arc::clone(&dead);
        let reader_shared = Arc::clone(&self.shared);
        let protocol = self.spec.payload_protocol;
        let Ok(reader_thread) = thread::Builder::new()
            .name("rp-svc-client-read".to_string())
            .spawn(move || read_replies(reader, protocol, &reader_shared, &reader_dead))
        else {
            return;
        };
        self.shared.state.store(STATE_UP, Ordering::Release);
        let mut next_health = Instant::now() + self.spec.health_interval;
        while !self.stopping() && !dead.load(Ordering::Acquire) {
            let wait = next_health
                .saturating_duration_since(Instant::now())
                .min(READ_POLL);
            match self.rx.recv_timeout(wait) {
                Ok(out) => {
                    if let Ok(mut set) = self.shared.in_flight.lock() {
                        set.insert(out.request_id);
                    }
                    let frame =
                        Frame::request(protocol, out.payload).with_request_id(out.request_id);
                    let written = encode_framed(&frame)
                        .map_err(io::Error::other)
                        .and_then(|wire| writer.write_all(&wire))
                        .and_then(|()| writer.flush());
                    if written.is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_health {
                match self.probe() {
                    Some(current) if current.pid == identity.pid => {}
                    _ => break,
                }
                next_health = Instant::now() + self.spec.health_interval;
            }
        }
        self.shared.state.store(STATE_DOWN, Ordering::Release);
        dead.store(true, Ordering::Release);
        drop(writer);
        let _ = reader_thread.join();
    }
}

fn read_replies(
    mut reader: crate::platform::ipc::Stream,
    protocol: u32,
    shared: &Shared,
    dead: &AtomicBool,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if dead.load(Ordering::Acquire) || shared.stop.load(Ordering::Acquire) {
            break;
        }
        match try_decode_framed(&buf) {
            Ok(Some(decoded)) => {
                buf.drain(..decoded.consumed);
                let frame = decoded.frame;
                if frame.payload_protocol != protocol
                    || FrameKind::try_from(frame.kind) != Ok(FrameKind::Response)
                {
                    continue;
                }
                let known = shared
                    .in_flight
                    .lock()
                    .map(|mut set| set.remove(&frame.request_id))
                    .unwrap_or(false);
                if known {
                    shared.push_reply(ServiceReply {
                        request_id: frame.request_id,
                        result: Ok(frame.payload),
                    });
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }
    dead.store(true, Ordering::Release);
}

/// Exclusive, non-blocking spawn lock; released on drop.
struct SpawnLock(File);

impl SpawnLock {
    fn try_acquire(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        match file.try_lock() {
            Ok(()) => Some(Self(file)),
            Err(TryLockError::WouldBlock) | Err(TryLockError::Error(_)) => None,
        }
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path) -> ServiceSpec {
        ServiceSpec::new("svc-unit", "1.0.0", 0xF0B7)
            .unwrap()
            .runtime_dir(Some(dir.to_path_buf()))
            .health_interval(Duration::from_millis(100))
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpsvc-{}-{}",
            std::process::id(),
            fnv1a32(format!("{:?}", Instant::now()).as_bytes())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn spec_validates_name_and_protocol() {
        assert!(ServiceSpec::new("Bad", "1", 0xF0B7).is_err());
        assert!(ServiceSpec::new("ok", "1", registry::ADMIN_PAYLOAD_PROTOCOL).is_err());
        assert!(ServiceSpec::new("ok", "1", 0x1234).is_err());
        assert!(ServiceSpec::new("ok", "1", 0x7001).is_ok());
    }

    #[test]
    fn round_trip_and_single_instance() {
        let dir = tempdir();
        let server = ServiceServer::bind(spec(&dir)).unwrap();
        assert!(matches!(
            ServiceServer::bind(spec(&dir)),
            Err(ServiceError::AlreadyRunning { .. })
        ));
        let worker = thread::spawn(move || {
            let request = server.recv(Some(Duration::from_secs(15))).unwrap();
            let mut out = b"pong:".to_vec();
            out.extend_from_slice(request.payload());
            request.reply(out).unwrap();
            server
        });
        let client = PollingFrameClient::connect(spec(&dir), 8).unwrap();
        wait_for(|| (client.state() == ServiceState::Up).then_some(()));
        let id = client.send(b"ping".to_vec()).unwrap();
        let reply = wait_for(|| client.try_recv());
        assert_eq!(reply.request_id, id);
        assert_eq!(reply.result, Ok(b"pong:ping".to_vec()));
        drop(worker.join().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn close_releases_endpoint_and_stops_client() {
        let dir = tempdir();
        let server = ServiceServer::bind(spec(&dir)).unwrap();
        assert_eq!(server.spec().name(), "svc-unit");
        assert!(spec(&dir).identity_path().exists());
        let client = PollingFrameClient::connect(spec(&dir), 8).unwrap();
        wait_for(|| (client.state() == ServiceState::Up).then_some(()));
        server.close();
        assert!(!spec(&dir).identity_path().exists());
        let again = ServiceServer::bind(spec(&dir)).expect("endpoint released");
        let started = Instant::now();
        client.close();
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(client.state(), ServiceState::Down);
        assert_eq!(client.send(b"x".to_vec()), Err(SendError::Unavailable));
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn send_refuses_immediately_without_a_service() {
        let dir = tempdir();
        let client = PollingFrameClient::connect(spec(&dir), 8).unwrap();
        let started = Instant::now();
        assert_eq!(client.send(b"x".to_vec()), Err(SendError::Unavailable));
        assert!(client.try_recv().is_none());
        assert!(started.elapsed() < Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(dir);
    }
}
