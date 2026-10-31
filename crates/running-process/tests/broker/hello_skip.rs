#![cfg(feature = "client")]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::client::{
    connect_to_backend, connect_to_backend_within, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::server::local_socket_name;

use crate::socket_common::unique_socket_name;

#[test]
fn connect_to_backend_direct_connects_to_cached_endpoint_when_versions_match() {
    let cached_backend = unique_socket_name("backend");
    let backend = spawn_accept_once(cached_backend.clone());

    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&cached_backend);
    let mut connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, cached_backend);
    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    assert!(connection.negotiated.is_none());
    connection.stream.write_all(&[0xA5]).unwrap();
    drop(connection.stream);
    backend.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_within_connects_to_cached_endpoint_inside_budget() {
    let cached_backend = unique_socket_name("backend-within");
    let backend = spawn_accept_once(cached_backend.clone());

    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&cached_backend);
    let mut connection = connect_to_backend_within(request, Duration::from_secs(1)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    connection.stream.write_all(&[0xA5]).unwrap();
    drop(connection.stream);
    backend.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_within_times_out_on_unresponsive_broker() {
    // The broker accepts the Hello connection and never replies. Without a
    // budget this waits for the internal 30 s Hello bound.
    let broker = unique_socket_name("stalled-broker");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel();
    let socket = broker.clone();
    let holder = thread::spawn(move || -> io::Result<()> {
        let listener = bind_test_socket(&socket)?;
        ready_tx.send(()).unwrap();
        let _stream = listener.accept()?;
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        cleanup_test_socket(&socket);
        Ok(())
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();

    let budget = Duration::from_millis(500);
    let request = ConnectBackendRequest::new(&broker, "zccache", "1.11.20", "1.11.19");
    let start = std::time::Instant::now();
    let error = connect_to_backend_within(request, budget).unwrap_err();
    let elapsed = start.elapsed();

    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    assert_eq!(error.connect_timed_out().map(|t| t.budget), Some(budget));
    match &error {
        running_process::broker::client::BrokerClientError::BackendConnect(source) => {
            assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        }
        other => panic!("expected BackendConnect(TimedOut), got {other:?}"),
    }
    release_tx.send(()).unwrap();
    holder.join().unwrap().unwrap();
}

fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = socket_name.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = match bind_test_socket(&socket_name) {
            Ok(listener) => {
                ready_tx.send(Ok(())).unwrap();
                listener
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err.to_string()));
                return Err(err);
            }
        };
        let mut stream = listener.accept()?;
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        assert_eq!(byte, [0xA5]);
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("failed to bind hello-skip backend socket {display_name}: {err}"),
        Err(err) => panic!("timed out waiting for hello-skip backend socket readiness: {err}"),
    }
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
