//! End-to-end handle-passing handoff through the production serve path
//! (#387).
//!
//! Runs the real `serve_registered_backend` accept loop with the opt-in
//! `handoff_endpoint` configured, a real endpoint-probe backend, and a
//! backend handoff listener speaking the production offer/ACK wire
//! protocol (`backend_lib::wire`). An opted-in `connect_to_backend`
//! client must end up with `BackendConnectionRoute::HandlePassed` and
//! serve traffic on the very socket that carried Hello; a backend that
//! rejects the offer must silently downgrade the client to the
//! `backend_pipe` reconnect (`BrokerNegotiated`) with no client-visible
//! error — the frozen correctness contract.

#![cfg(feature = "client")]

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Listener as _;
use prost::Message;
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_lib::wire::{read_handoff_offer, respond_to_handoff_offer};
use running_process::broker::client::{
    connect_to_backend, BackendConnection, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{
    BrokerIsolation, Endpoint, HandoffOffer, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, serve_registered_backend, service_definition_path,
    BrokerInstanceKey, BrokerServeConfig, HandoffToken, HandoffTokenStore, HANDOFF_TOKEN_BYTES,
};

use crate::backend_handle_common::spawn_endpoint_probe_once;
use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, bind_test_socket, cleanup_test_socket,
    unique_socket_name,
};

pub(crate) const CLIENT_PROBE: u8 = 0xC3;
pub(crate) const BACKEND_REPLY: u8 = 0x5A;

#[test]
fn serve_handoff_completes_and_client_adopts_connection() {
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("handoff-serve-ok");
    // No listener is ever re-bound on the backend endpoint after the
    // startup probe: a wrong fallback to reconnect would fail loudly.
    let backend_endpoint = unique_socket_name("handoff-serve-ok-be");
    let handoff_endpoint = unique_socket_name("handoff-serve-ok-ho");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let handoff_backend =
        spawn_backend_handoff_listener(handoff_endpoint.clone(), BackendBehavior::Accept);
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint.clone(),
        1,
    )
    .with_handoff_endpoint(handoff_endpoint);
    let server = thread::spawn(move || serve_registered_backend(config));

    let mut connection = connect_backend_with_retry(&socket_name, Duration::from_secs(10));

    assert_eq!(connection.route, BackendConnectionRoute::HandlePassed);
    assert_eq!(
        connection.endpoint, backend_endpoint,
        "adopted connections must still report backend_pipe for hello-skip caching"
    );
    assert!(connection.handoff_token().is_some());

    // Prove the SAME socket that carried Hello now serves backend traffic.
    connection.stream.write_all(&[CLIENT_PROBE]).unwrap();
    let mut reply = [0_u8; 1];
    connection.stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [BACKEND_REPLY]);

    drop(connection.stream);
    server.join().unwrap().unwrap();
    backend_probe.join().unwrap().unwrap();
    handoff_backend.join().unwrap().unwrap();
}

#[test]
fn rejected_handoff_silently_downgrades_to_backend_pipe() {
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("handoff-serve-rej");
    let backend_endpoint = unique_socket_name("handoff-serve-rej-be");
    let handoff_endpoint = unique_socket_name("handoff-serve-rej-ho");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let handoff_backend =
        spawn_backend_handoff_listener(handoff_endpoint.clone(), BackendBehavior::Reject);
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint.clone(),
        1,
    )
    .with_handoff_endpoint(handoff_endpoint);
    let server = thread::spawn(move || serve_registered_backend(config));
    // The startup probe owns the backend endpoint until verification ends;
    // only then can the reconnect listener take its place.
    backend_probe.join().unwrap().unwrap();
    let reconnect_backend = spawn_reconnect_accept_once(backend_endpoint.clone());

    let connection = connect_backend_with_retry(&socket_name, Duration::from_millis(300));

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    drop(connection.stream);
    server.join().unwrap().unwrap();
    handoff_backend.join().unwrap().unwrap();
    reconnect_backend.join().unwrap().unwrap();
}

/// What the test backend does with the broker's handoff offer.
pub(crate) enum BackendBehavior {
    /// Echo-accept the offered token, adopt the transferred connection,
    /// and serve one probe/reply byte exchange on it.
    Accept,
    /// Reject the offer (expected-token mismatch) with a well-formed
    /// `accepted = false` ACK.
    Reject,
}

/// Bind the backend handoff endpoint and serve one offer/ACK exchange
/// using the production backend-side wire helpers.
fn spawn_backend_handoff_listener(
    handoff_endpoint: String,
    behavior: BackendBehavior,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = handoff_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&handoff_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let result = serve_one_handoff(&mut stream, &behavior);
        cleanup_test_socket(&handoff_endpoint);
        result
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

#[cfg(windows)]
pub(crate) fn serve_one_handoff(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    let offer = read_handoff_offer(stream).map_err(io::Error::other)?;
    let handle_value = offer.handle_value;
    respond(stream, behavior, offer)?;
    if matches!(behavior, BackendBehavior::Accept) {
        // Adopt the handle the broker duplicated into this (backend)
        // process and prove it serves the client's connection. The pipe
        // was created overlapped by the broker's listener, so the byte
        // exchange uses explicit OVERLAPPED I/O on the raw handle.
        let adopted = unsafe { OwnedHandle::from_raw_handle(handle_value as RawHandle) };
        let mut probe = [0_u8; 1];
        overlapped_transfer(adopted.as_raw_handle(), &mut probe, false)?;
        if probe != [CLIENT_PROBE] {
            return Err(io::Error::other(
                "unexpected probe byte on adopted connection",
            ));
        }
        overlapped_transfer(adopted.as_raw_handle(), &mut [BACKEND_REPLY], true)?;
    }
    Ok(())
}

/// One blocking overlapped read (`write == false`) or write (`write ==
/// true`) on a duplicated overlapped pipe handle.
#[cfg(windows)]
fn overlapped_transfer(
    handle: std::os::windows::io::RawHandle,
    buffer: &mut [u8],
    write: bool,
) -> io::Result<()> {
    use winapi::shared::winerror::ERROR_IO_PENDING;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::fileapi::{ReadFile, WriteFile};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::ioapiset::GetOverlappedResult;
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::synchapi::CreateEventW;

    unsafe {
        let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.hEvent = event;
        let mut transferred = 0_u32;
        let immediate = if write {
            WriteFile(
                handle.cast(),
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        } else {
            ReadFile(
                handle.cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        };
        let result = if immediate != 0 {
            Ok(())
        } else if GetLastError() == ERROR_IO_PENDING {
            if GetOverlappedResult(handle.cast(), &mut overlapped, &mut transferred, 1) != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        } else {
            Err(io::Error::last_os_error())
        };
        CloseHandle(event);
        result?;
        if transferred as usize != buffer.len() {
            return Err(io::Error::other("short overlapped pipe transfer"));
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn serve_one_handoff(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
) -> io::Result<()> {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;

    // The fd plus token ride SCM_RIGHTS on the same handoff connection
    // that then carries the offer frame.
    let socket_fd = match &*stream {
        interprocess::local_socket::Stream::UdSocket(socket) => socket.as_fd().as_raw_fd(),
    };
    let (received_fd, _token) = recv_fd_and_token(socket_fd)?;
    let offer = read_handoff_offer(stream).map_err(io::Error::other)?;
    respond(stream, behavior, offer)?;
    match behavior {
        BackendBehavior::Accept => {
            let mut adopted = unsafe { UnixStream::from_raw_fd(received_fd) };
            serve_probe_reply(&mut adopted)?;
        }
        BackendBehavior::Reject => unsafe {
            libc::close(received_fd);
        },
    }
    Ok(())
}

/// Answer one offer through the production backend wire path: accepted
/// with the token seeded as pending, rejected via expected-token mismatch.
fn respond<S: Write>(
    stream: &mut S,
    behavior: &BackendBehavior,
    offer: HandoffOffer,
) -> io::Result<()> {
    let now = Instant::now();
    let mut pending_tokens = HandoffTokenStore::new();
    let expected_token = match behavior {
        BackendBehavior::Accept => {
            let bytes = <[u8; HANDOFF_TOKEN_BYTES]>::try_from(offer.token.as_slice())
                .map_err(|_| io::Error::other("offered token is not 16 bytes"))?;
            pending_tokens
                .issue_with_random128(now, || Ok(bytes))
                .map_err(io::Error::other)?
        }
        BackendBehavior::Reject => HandoffToken::from_bytes([0xEE; HANDOFF_TOKEN_BYTES]),
    };
    respond_to_handoff_offer(stream, &mut pending_tokens, expected_token, offer, now)
        .map_err(io::Error::other)?;
    Ok(())
}

#[cfg(unix)]
fn serve_probe_reply<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    let mut probe = [0_u8; 1];
    stream.read_exact(&mut probe)?;
    if probe != [CLIENT_PROBE] {
        return Err(io::Error::other(
            "unexpected probe byte on adopted connection",
        ));
    }
    stream.write_all(&[BACKEND_REPLY])
}

#[cfg(unix)]
fn recv_fd_and_token(
    socket_fd: std::os::fd::RawFd,
) -> io::Result<(std::os::fd::RawFd, [u8; HANDOFF_TOKEN_BYTES])> {
    let mut token = [0_u8; HANDOFF_TOKEN_BYTES];
    let mut iov = libc::iovec {
        iov_base: token.as_mut_ptr().cast(),
        iov_len: token.len(),
    };
    let mut control =
        vec![0_u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize }];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;

    let received = unsafe { libc::recvmsg(socket_fd, &mut message, 0) };
    if received as usize != token.len() {
        return Err(io::Error::other("short SCM_RIGHTS handoff read"));
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        return Err(io::Error::other("missing SCM_RIGHTS control message"));
    }
    unsafe {
        if (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("unexpected handoff control message"));
        }
        Ok((*libc::CMSG_DATA(header).cast::<libc::c_int>(), token))
    }
}

/// Accept one reconnect on the backend endpoint, retrying the bind while
/// the just-closed startup-probe listener still holds the pipe name.
fn spawn_reconnect_accept_once(backend_endpoint: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = backend_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let listener = loop {
            match bind_test_socket(&backend_endpoint) {
                Ok(listener) => break listener,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return Err(error);
                }
            }
        };
        ready_tx.send(Ok(())).unwrap();
        let _stream = listener.accept()?;
        cleanup_test_socket(&backend_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

/// Opted-in client connect, retrying only while the broker socket is not
/// yet bound. Each successful dial performs one full Hello negotiation.
fn connect_backend_with_retry(broker_endpoint: &str, ready_timeout: Duration) -> BackendConnection {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut request =
            ConnectBackendRequest::new(broker_endpoint, "zccache", "1.11.20", "1.11.20");
        request.adopt_handed_off_connection = true;
        request.handoff_ready_timeout = ready_timeout;
        match connect_to_backend(request) {
            Ok(connection) => return connection,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("timed out connecting through broker {broker_endpoint}: {err}");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

pub(crate) fn serve_config(
    service_root: &Path,
    socket_path: String,
    backend_endpoint: String,
    max_connections: usize,
) -> BrokerServeConfig {
    BrokerServeConfig::new(
        socket_path,
        "zccache",
        "1.11.20",
        backend_endpoint,
        max_connections,
    )
    .unwrap()
    .with_service_definition_dir(service_root)
}

pub(crate) fn spawn_configured_backend_probe(
    backend_endpoint: &str,
) -> thread::JoinHandle<io::Result<()>> {
    let endpoint = Endpoint {
        namespace_id: BrokerInstanceKey::Shared.id(),
        path: backend_endpoint.into(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).unwrap();
    spawn_endpoint_probe_once(daemon)
}

fn service_definition() -> ServiceDefinition {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: exe.to_string_lossy().into_owned(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: dir.to_string_lossy().into_owned(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

pub(crate) fn write_service_definition_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    let path = service_definition_path(&root, "zccache").unwrap();
    fs::write(path, service_definition().encode_to_vec()).unwrap();
    tmp
}
