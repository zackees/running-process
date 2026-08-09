//! Slice-3b tests (soldr#2365): drive a **contained** child (`SpawnedChild`, in
//! its own Job Object / process group) through the pump + codec, and end-to-end
//! through [`serve_session`] over a real in-process byte transport. Both assert
//! byte-equality against a direct-execution oracle, so containment and the
//! transport bridge add proxying, never touch fidelity.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use super::{serve_session, spawn_contained_session};
use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};
use crate::broker::session_codec::{
    encode_session_frame, try_decode_session_frame, DecodedSessionFrame,
};
use crate::broker::session_pump::run_child_session;
use crate::containment::ContainedProcessGroup;

/// A blocking in-memory pipe (one direction): `read` blocks until bytes arrive
/// or the writer is dropped (EOF). Dependency-free and MSRV-safe, it gives the
/// session server a genuine concurrent byte boundary without a socket.
mod mempipe {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::{Arc, Condvar, Mutex};

    struct Shared {
        buf: VecDeque<u8>,
        writer_open: bool,
    }

    type Chan = Arc<(Mutex<Shared>, Condvar)>;

    pub struct Reader(Chan);
    pub struct Writer(Chan);

    pub fn pipe() -> (Reader, Writer) {
        let chan = Arc::new((
            Mutex::new(Shared {
                buf: VecDeque::new(),
                writer_open: true,
            }),
            Condvar::new(),
        ));
        (Reader(chan.clone()), Writer(chan))
    }

    impl Read for Reader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let (lock, cv) = &*self.0;
            let mut guard = lock.lock().unwrap();
            loop {
                if !guard.buf.is_empty() {
                    let n = out.len().min(guard.buf.len());
                    for slot in out.iter_mut().take(n) {
                        *slot = guard.buf.pop_front().unwrap();
                    }
                    return Ok(n);
                }
                if !guard.writer_open {
                    return Ok(0);
                }
                guard = cv.wait(guard).unwrap();
            }
        }
    }

    impl Write for Writer {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let (lock, cv) = &*self.0;
            lock.lock().unwrap().buf.extend(data.iter().copied());
            cv.notify_all();
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Writer {
        fn drop(&mut self) {
            let (lock, cv) = &*self.0;
            lock.lock().unwrap().writer_open = false;
            cv.notify_all();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Reassembled {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: (i32, i32),
}

fn fixture() -> Command {
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.is_file(),
        "test fixture is missing at {} — run `soldr cargo build -p testbins` first",
        path.display()
    );
    Command::new(path)
}

fn frame(kind: session_frame::Kind) -> SessionFrame {
    SessionFrame { kind: Some(kind) }
}

/// Run the fixture directly (the oracle/golden) and capture the triple.
fn run_direct(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let mut child = fixture()
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture directly");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    Reassembled {
        stdout: output.stdout,
        stderr: output.stderr,
        exit: (output.status.code().unwrap_or(-1), 0),
    }
}

/// Reassemble already-decoded outbound frames from the pump.
fn collect(out_rx: Receiver<SessionFrame>, exit: SessionExit) -> Reassembled {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_frame = None;
    for frame in out_rx {
        match frame.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => exit_frame = Some((e.code, e.signal)),
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }
    let exit_frame = exit_frame.expect("a terminal Exit frame is always sent");
    assert_eq!(exit_frame, (exit.code, exit.signal));
    Reassembled {
        stdout,
        stderr,
        exit: exit_frame,
    }
}

/// Drive a **contained** child through the pump directly (no transport): proves
/// the `SessionChild` impl for `SpawnedChild` + Job Object containment produce
/// the same bytes as the oracle.
fn run_contained_through_pump(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let group = ContainedProcessGroup::new().expect("contained group");
    let child = spawn_contained_session(&group, fixture().args(directives))
        .expect("contained spawn with piped stdio");

    let (stdin_tx, stdin_rx) = channel();
    if !stdin.is_empty() {
        stdin_tx
            .send(frame(session_frame::Kind::Stdin(stdin.to_vec())))
            .unwrap();
    }
    stdin_tx
        .send(frame(session_frame::Kind::StdinEof(true)))
        .unwrap();
    drop(stdin_tx);

    let (out_tx, out_rx) = channel();
    let exit = run_child_session(child, out_tx, stdin_rx).expect("pump reaps contained child");
    collect(out_rx, exit)
}

/// Full path: a contained child served through [`serve_session`] over two
/// in-memory pipes, with a client thread that speaks the SESSION codec.
fn run_over_transport(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let group = ContainedProcessGroup::new().expect("contained group");
    let child = spawn_contained_session(&group, fixture().args(directives))
        .expect("contained spawn with piped stdio");

    let (server_in, client_in) = mempipe::pipe(); // client → server (stdin frames)
    let (client_out, server_out) = mempipe::pipe(); // server → client (output frames)

    let stdin_vec = stdin.to_vec();
    let client = thread::spawn(move || -> Reassembled {
        let mut client_in = client_in;
        let mut seq = 0u64;
        if !stdin_vec.is_empty() {
            let wire =
                encode_session_frame(&frame(session_frame::Kind::Stdin(stdin_vec)), seq).unwrap();
            seq += 1;
            client_in.write_all(&wire).unwrap();
        }
        let eof = encode_session_frame(&frame(session_frame::Kind::StdinEof(true)), seq).unwrap();
        client_in.write_all(&eof).unwrap();
        drop(client_in); // close inbound → server sees EOF

        // Read the outbound stream to EOF and decode it.
        let mut client_out = client_out;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_frame = None;
        loop {
            while let Some(DecodedSessionFrame { frame, consumed }) =
                try_decode_session_frame(&buf).expect("decode ok")
            {
                match frame.kind {
                    Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
                    Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
                    Some(session_frame::Kind::Exit(e)) => exit_frame = Some((e.code, e.signal)),
                    _ => panic!("unexpected frame on outbound stream"),
                }
                buf.drain(..consumed);
            }
            let n = client_out.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Reassembled {
            stdout,
            stderr,
            exit: exit_frame.expect("a terminal Exit frame is always sent"),
        }
    });

    let exit = serve_session(child, server_in, server_out).expect("serve_session reaps child");
    let reassembled = client.join().expect("client thread");
    assert_eq!(
        reassembled.exit,
        (exit.code, exit.signal),
        "the client-visible Exit must match serve_session's return"
    );
    reassembled
}

#[test]
fn contained_child_through_pump_matches_oracle() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(
        run_contained_through_pump(script, b""),
        run_direct(script, b"")
    );
}

#[test]
fn contained_child_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let got = run_contained_through_pump(script, b"");
    assert_eq!(got.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(got.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(got, run_direct(script, b""));
}

#[test]
fn serve_session_over_transport_matches_oracle() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(run_over_transport(script, b""), run_direct(script, b""));
}

#[test]
fn serve_session_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let got = run_over_transport(script, b"");
    assert_eq!(got.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(got.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(got, run_direct(script, b""));
}

#[test]
fn serve_session_delivers_full_byte_range_stdin() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let got = run_over_transport(&["echo"], &payload);
    assert_eq!(got.stdout, payload);
    assert_eq!(got, run_direct(&["echo"], &payload));
}

#[test]
fn serve_session_reports_nonzero_exit() {
    let got = run_over_transport(&["err:boom", "exit:7"], b"");
    assert_eq!(got.exit, (7, 0));
    assert_eq!(got.stderr, b"boom");
}
