#![cfg(feature = "client")]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::client::send_admin_request;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, write_frame, AdminReplyKind, AdminRequest,
    AdminVerb, BrokerIsolation, Endpoint, ErrorCode, Frame, FrameKind, Hello, HelloReply,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    build_hello_handler, ensure_service_definition_dir, local_socket_name,
    serve_launching_backends_with_launcher, serve_registered_backend, service_definition_path,
    BackendLaunchError, BackendLaunchRequest, BackendLauncher, BrokerLaunchServeConfig,
    BrokerServeConfig, PeerIdentity,
};
use serde_json::Value;

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition() -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir,
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn write_definition_for(root: &Path, service_name: &str, definition: &ServiceDefinition) {
    let path = service_definition_path(root, service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

fn write_service_definition_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, "zccache", &service_definition());
    tmp
}

fn serve_config(
    service_root: &Path,
    socket_path: impl Into<String>,
    backend_endpoint: impl Into<String>,
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

fn launch_serve_config(
    service_root: &Path,
    socket_path: impl Into<String>,
    max_connections: usize,
) -> BrokerLaunchServeConfig {
    BrokerLaunchServeConfig::new(socket_path, max_connections)
        .unwrap()
        .with_service_definition_dir(service_root)
}

fn hello(peer_pid: u32) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-serve".into(),
        connection_id: 0,
        peer_pid,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn frame_for_hello(request: &Hello) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: request.encode_to_vec(),
        request_id: 99,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

#[test]
fn build_hello_handler_uses_service_definition_and_backend_registry() {
    let tmp = write_service_definition_dir();
    let backend_endpoint = unique_backend_endpoint();
    let config = serve_config(
        &tmp.path().join("services"),
        "unused-test-socket",
        backend_endpoint.clone(),
        1,
    );

    let handler = build_hello_handler(&config).unwrap();
    let reply = handler.handle_frame(
        frame_for_hello(&hello(0)),
        PeerIdentity {
            pid: 0,
            uid_or_sid: "test-peer".into(),
        },
    );

    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, backend_endpoint);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn serve_registered_backend_round_trips_loaded_service_definition() {
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name();
    let backend_endpoint = unique_backend_endpoint();
    let config = serve_config(
        &tmp.path().join("services"),
        socket_name.clone(),
        backend_endpoint.clone(),
        1,
    );
    let server = thread::spawn(move || serve_registered_backend(config));

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let mut client = connect_with_retry(name);
    let request_frame = frame_for_hello(&hello(0));
    write_frame(&mut client, &request_frame.encode_to_vec()).unwrap();

    let response_bytes = read_frame(&mut client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();

    server.join().unwrap().unwrap();
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    assert_eq!(response_frame.request_id, 99);
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, backend_endpoint);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn serve_registered_backend_rereads_service_definition_for_accepted_hello() {
    let tmp = write_service_definition_dir();
    let service_root = tmp.path().join("services");
    let socket_name = unique_socket_name();
    let backend_endpoint = unique_backend_endpoint();
    let config = serve_config(
        &service_root,
        socket_name.clone(),
        backend_endpoint.clone(),
        1,
    );
    let server = thread::spawn(move || serve_registered_backend(config));

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let mut client = connect_with_retry(name);
    let mut updated = service_definition();
    updated.min_version = "1.12.0".into();
    write_definition_for(&service_root, "zccache", &updated);

    let request_frame = frame_for_hello(&hello(0));
    write_frame(&mut client, &request_frame.encode_to_vec()).unwrap();
    let response_bytes = read_frame(&mut client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();

    server.join().unwrap().unwrap();
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    assert_eq!(response_frame.request_id, 99);
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => {
            assert_eq!(
                ErrorCode::try_from(refused.code),
                Ok(ErrorCode::ErrorVersionBlocked)
            );
            assert_eq!(refused.reason, "wanted_version is below min_version");
        }
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected version refusal, got {negotiated:?}")
        }
    }
}

#[test]
fn serve_launching_backends_launches_once_then_reuses_registry() {
    let tmp = write_service_definition_dir();
    let service_root = tmp.path().join("services");
    let socket_name = unique_socket_name();
    let backend_endpoint = unique_backend_endpoint();
    let launcher = Arc::new(CurrentProcessLauncher::new(backend_endpoint.clone()));
    let server_launcher = Arc::clone(&launcher);
    let config = launch_serve_config(&service_root, socket_name.clone(), 2);
    let server = thread::spawn(move || {
        serve_launching_backends_with_launcher(config, server_launcher.as_ref())
    });

    let first = send_hello_roundtrip(&socket_name);
    let second = send_hello_roundtrip(&socket_name);

    server.join().unwrap().unwrap();
    assert_negotiated_backend(first, &backend_endpoint);
    assert_negotiated_backend(second, &backend_endpoint);
    assert_eq!(launcher.launch_count(), 1);
}

#[test]
fn serve_launching_backends_serves_admin_on_same_socket() {
    let tmp = write_service_definition_dir();
    let service_root = tmp.path().join("services");
    let socket_name = unique_socket_name();
    let backend_endpoint = unique_backend_endpoint();
    let launcher = Arc::new(CurrentProcessLauncher::new(backend_endpoint.clone()));
    let server_launcher = Arc::clone(&launcher);
    let config = launch_serve_config(&service_root, socket_name.clone(), 2);
    let server = thread::spawn(move || {
        serve_launching_backends_with_launcher(config, server_launcher.as_ref())
    });

    let hello_reply = send_hello_roundtrip(&socket_name);
    let admin_reply = send_admin_request(
        &socket_name,
        AdminRequest {
            verb: AdminVerb::Status as i32,
            json: true,
            service_name: String::new(),
            output_path: String::new(),
        },
    )
    .unwrap();

    server.join().unwrap().unwrap();
    assert_negotiated_backend(hello_reply, &backend_endpoint);
    assert_eq!(launcher.launch_count(), 1);
    assert_eq!(
        AdminReplyKind::try_from(admin_reply.kind),
        Ok(AdminReplyKind::Json)
    );
    let value: Value = serde_json::from_str(&admin_reply.body).unwrap();
    assert_eq!(value["command"], "status");
    assert_eq!(value["backends"][0]["service_name"], "zccache");
    assert_eq!(value["backends"][0]["service_version"], "1.11.20");
    assert_eq!(value["backends"][0]["backend_pipe"], backend_endpoint);
}

fn send_hello_roundtrip(socket_name: &str) -> HelloReply {
    let name = local_socket_name(socket_name).unwrap().into_owned();
    let mut client = connect_with_retry(name);
    let request_frame = frame_for_hello(&hello(0));
    write_frame(&mut client, &request_frame.encode_to_vec()).unwrap();

    let response_bytes = read_frame(&mut client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    HelloReply::decode(response_frame.payload.as_slice()).unwrap()
}

fn assert_negotiated_backend(reply: HelloReply, expected_endpoint: &str) {
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, expected_endpoint);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

struct CurrentProcessLauncher {
    endpoint_path: String,
    launch_count: AtomicUsize,
}

impl CurrentProcessLauncher {
    fn new(endpoint_path: impl Into<String>) -> Self {
        Self {
            endpoint_path: endpoint_path.into(),
            launch_count: AtomicUsize::new(0),
        }
    }

    fn launch_count(&self) -> usize {
        self.launch_count.load(Ordering::SeqCst)
    }
}

impl BackendLauncher for CurrentProcessLauncher {
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        self.launch_count.fetch_add(1, Ordering::SeqCst);
        let endpoint = Endpoint {
            namespace_id: request.key.instance.id(),
            path: self.endpoint_path.clone(),
        };
        let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30))?;
        Ok(BackendHandle::probe_with_service(
            request.key.service_name.clone(),
            request.key.service_version.clone(),
            &endpoint,
            &daemon,
        )?)
    }
}

fn connect_with_retry(
    name: interprocess::local_socket::Name<'static>,
) -> interprocess::local_socket::Stream {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match LocalSocketStream::connect(name.borrow()) {
            Ok(stream) => return stream,
            Err(err) if Instant::now() < deadline => {
                if !is_pending_bind_error(&err) {
                    panic!("failed to connect to broker local socket: {err}");
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("timed out connecting to broker local socket: {err}"),
        }
    }
}

fn is_pending_bind_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    )
}

#[cfg(windows)]
fn unique_socket_name() -> String {
    format!(
        "rpb-v1-serve-mode-{}-{}",
        std::process::id(),
        unique_suffix()
    )
}

#[cfg(unix)]
fn unique_socket_name() -> String {
    std::env::temp_dir()
        .join(format!(
            "rpb-v1-serve-mode-{}-{}.sock",
            std::process::id(),
            unique_suffix()
        ))
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn unique_backend_endpoint() -> String {
    format!(
        "rpb-v1-backend-endpoint-{}-{}",
        std::process::id(),
        unique_suffix()
    )
}

#[cfg(unix)]
fn unique_backend_endpoint() -> String {
    std::env::temp_dir()
        .join(format!(
            "rpb-v1-backend-endpoint-{}-{}.sock",
            std::process::id(),
            unique_suffix()
        ))
        .to_string_lossy()
        .into_owned()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
