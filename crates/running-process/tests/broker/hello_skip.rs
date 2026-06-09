#![cfg(feature = "client")]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::server::local_socket_name;

#[test]
fn connect_to_backend_direct_connects_to_cached_endpoint_when_versions_match() {
    let cached_backend = unique_socket_name("hello-skip-backend");
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

fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_test_socket(&socket_name)?;
        ready_tx.send(()).unwrap();
        let mut stream = listener.accept()?;
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        assert_eq!(byte, [0xA5]);
        cleanup_test_socket(&socket_name);
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
    let dir = if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    dir.join(format!(
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
