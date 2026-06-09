#![cfg(feature = "client")]

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    handle_hello_connection, local_socket_name, HelloHandler, PeerIdentity, RegisteredBackend,
};

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handler(backend_endpoint: &str) -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: backend_endpoint.into(),
            server_capabilities: 0x01,
        })
        .unwrap()
}

#[test]
fn connect_to_backend_uses_cached_endpoint_when_versions_match() {
    let cached_backend = unique_socket_name("cached-backend");
    let backend = spawn_accept_once(cached_backend.clone());

    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&cached_backend);
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, cached_backend);
    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    assert!(connection.negotiated.is_none());
    drop(connection.stream);
    backend.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_falls_back_to_broker_when_cache_missing() {
    let broker_endpoint = unique_socket_name("broker");
    let backend_endpoint = unique_socket_name("backend");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(
        connection.negotiated.as_ref().unwrap().daemon_version,
        "1.11.20"
    );
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_falls_back_to_broker_when_cached_endpoint_is_stale() {
    let broker_endpoint = unique_socket_name("broker-stale-cache");
    let backend_endpoint = unique_socket_name("backend-stale-cache");
    let stale_cached_endpoint = unique_socket_name("stale-cached-backend");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let mut request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&stale_cached_endpoint);
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(
        connection.negotiated.as_ref().unwrap().daemon_version,
        "1.11.20"
    );
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_does_not_skip_when_versions_differ() {
    let broker_endpoint = unique_socket_name("broker-mismatch");
    let backend_endpoint = unique_socket_name("backend-mismatch");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let mut request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.19");
    request.cached_backend_endpoint = Some("missing-cached-backend");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_test_socket(&socket_name)?;
        ready_tx.send(()).unwrap();
        let _stream = listener.accept()?;
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    handle
}

fn spawn_broker_once(
    broker_endpoint: String,
    backend_endpoint: String,
) -> thread::JoinHandle<io::Result<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_test_socket(&broker_endpoint)?;
        ready_tx.send(()).unwrap();
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "test-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler(&backend_endpoint), peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_endpoint);
        Ok(())
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    handle
}

fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name)?;
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

fn prepare_test_socket(socket_name: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_name;

    Ok(())
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

#[cfg(windows)]
fn unique_socket_name(label: &str) -> String {
    format!("rpb-v1-{label}-{}-{}", std::process::id(), unique_suffix())
}

#[cfg(unix)]
fn unique_socket_name(label: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "rpb-v1-{label}-{}-{}.sock",
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
