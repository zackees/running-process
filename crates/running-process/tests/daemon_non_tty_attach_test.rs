//! Non-TTY attach degraded mode (#130 M6 C9).
//!
//! When a client attaches with `is_tty=false`, the daemon skips the
//! resize side effect (pixel dimensions are meaningless without a real
//! terminal) and records the flag + TERM string so list responses can
//! surface them.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pty_session::{PtyAttachment, PtySpawnRequest};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::AttachPtySessionRequest;

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", "testbins", "--bin", name, "--message-format=json"])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo build failed");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(name) {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v["reason"] == "compiler-artifact"
                && v["target"]["kind"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|k| k == "bin"))
            {
                if let Some(exe) = v["executable"].as_str() {
                    let p = PathBuf::from(exe);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !p.exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    return p;
                }
            }
        }
    }
    panic!("could not find binary artifact for {name}");
}

fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();
    let server = DaemonServer::new(
        socket.clone(),
        db,
        "non-tty-attach-test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("DaemonServer::new");
    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run");
    });
    (handle, socket)
}

/// Open a raw socket connection to the daemon and send a manually-built
/// AttachPtySessionRequest with is_tty=false. The high-level
/// PtyAttachment helper always sets is_tty=true (it represents the
/// common interactive case), so this test goes one layer below the
/// helper to exercise the non-TTY path.
fn raw_attach_non_tty(
    socket_path: &str,
    session_id: &str,
    rows: u32,
    cols: u32,
    term: &str,
) -> std::io::Result<BufReader<interprocess::local_socket::Stream>> {
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::Stream;
    use interprocess::TryClone;
    use prost::Message;
    use running_process::proto::daemon::{DaemonRequest, DaemonResponse, RequestType, StatusCode};

    let name = paths::make_socket_name(socket_path)?;
    let stream = Stream::connect(name)?;
    let stream_clone = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(stream_clone);

    let req = DaemonRequest {
        id: 1,
        r#type: RequestType::AttachPtySession.into(),
        protocol_version: 1,
        client_name: "non-tty-attach-test".into(),
        attach_pty_session: Some(AttachPtySessionRequest {
            session_id: session_id.into(),
            rows,
            cols,
            steal: false,
            term: term.into(),
            is_tty: false,
        }),
        ..Default::default()
    };
    let bytes = req.encode_to_vec();
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;

    // Read response header so the test can ensure the attach succeeded
    // before returning. (We do not consume further stream frames.)
    use std::io::Read;
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_buf = vec![0u8; resp_len];
    reader.read_exact(&mut resp_buf)?;
    let resp = DaemonResponse::decode(&resp_buf[..]).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("decode: {e}"))
    })?;
    if resp.code != StatusCode::Ok as i32 {
        return Err(std::io::Error::other(format!(
            "attach failed: {} ({})",
            resp.message, resp.code
        )));
    }
    Ok(reader)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_tty_attach_records_flag_and_skips_resize() {
    let scope = format!("non-tty-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn at known dimensions.
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("non-tty-attach")
                    .with_size(40, 100),
            )
            .expect("spawn");

        // Sanity: before any attach, dimensions match what we asked.
        let listed = client.list_pty_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert_eq!(entry.rows, 40);
        assert_eq!(entry.cols, 100);

        // Non-TTY attach with very different rows/cols. The dimensions
        // should be IGNORED (non-TTY clients have no terminal size).
        // Keep the connection's reader alive so the attachment stays
        // active while we inspect the daemon state.
        let _attach_reader =
            raw_attach_non_tty(&socket_for_test, &session.session_id, 999, 999, "dumb")
                .expect("non-tty attach");

        // Brief wait so the daemon installs the attachment.
        std::thread::sleep(Duration::from_millis(100));

        let listed = client.list_pty_sessions("").expect("list after attach");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert!(entry.attached);
        assert!(
            !entry.attached_is_tty,
            "attached_is_tty must be false for non-TTY clients"
        );
        assert_eq!(
            entry.attached_term, "dumb",
            "TERM should be recorded as supplied"
        );
        assert_eq!(
            entry.rows, 40,
            "non-TTY attach must NOT resize (rows unchanged)"
        );
        assert_eq!(
            entry.cols, 100,
            "non-TTY attach must NOT resize (cols unchanged)"
        );

        // Compare against the TTY path: a regular PtyAttachment uses
        // is_tty=true and should resize the session. Drop the non-TTY
        // attachment first so we can re-attach.
        drop(_attach_reader);
        std::thread::sleep(Duration::from_millis(100));

        let _tty_attachment =
            PtyAttachment::attach_to(&socket_for_test, &session.session_id, 12, 34, true)
                .expect("tty attach");
        std::thread::sleep(Duration::from_millis(100));
        let listed = client.list_pty_sessions("").expect("list after tty attach");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert!(entry.attached_is_tty);
        assert_eq!(entry.rows, 12);
        assert_eq!(entry.cols, 34);

        client
            .terminate_pty_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
