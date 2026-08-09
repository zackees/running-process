use super::*;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;

fn testbin_path(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture is missing at {} — run `soldr cargo build -p testbins` first",
        path.display()
    );
    path
}

/// Reassemble a proxied session's frame stream back into the (stdout, stderr,
/// exit) triple — the shape the direct oracle produces, so the two are
/// comparable byte-for-byte.
#[derive(Debug, PartialEq, Eq)]
struct Reassembled {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: (i32, i32),
}

/// Run the fixture through the pump: spawn it with piped stdio, feed `stdin`
/// (then EOF), drive `run_child_session`, and reassemble the emitted frames.
fn run_through_pump(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture under pump");

    let (stdin_tx, stdin_rx) = channel();
    if !stdin.is_empty() {
        stdin_tx
            .send(SessionFrame {
                kind: Some(session_frame::Kind::Stdin(stdin.to_vec())),
            })
            .unwrap();
    }
    stdin_tx
        .send(SessionFrame {
            kind: Some(session_frame::Kind::StdinEof(true)),
        })
        .unwrap();
    drop(stdin_tx); // close the inbound lane

    let (out_tx, out_rx) = channel();
    let exit = run_child_session(child, out_tx, stdin_rx).expect("pump reaps child");

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
    assert_eq!(
        exit_frame,
        (exit.code, exit.signal),
        "the Exit frame must match the returned SessionExit"
    );
    Reassembled {
        stdout,
        stderr,
        exit: exit_frame,
    }
}

/// Run the fixture directly (the oracle/golden) and capture the same triple.
fn run_direct(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let mut child = Command::new(testbin_path("testbin-stdio-scripted"))
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

#[test]
fn pump_reproduces_direct_stdout_stderr_and_exit() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(run_through_pump(script, b""), run_direct(script, b""));
}

#[test]
fn pump_keeps_stdout_and_stderr_on_their_own_streams() {
    // Interleaved writes must not cross streams once proxied.
    let script = &["out:1", "err:2", "out:3", "err:4", "out:5"];
    let pumped = run_through_pump(script, b"");
    assert_eq!(pumped.stdout, b"135");
    assert_eq!(pumped.stderr, b"24");
    assert_eq!(pumped, run_direct(script, b""));
}

#[test]
fn pump_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let pumped = run_through_pump(script, b"");
    assert_eq!(pumped.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(pumped.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(pumped, run_direct(script, b""));
}

#[test]
fn pump_delivers_stdin_to_the_child_verbatim() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let pumped = run_through_pump(&["echo"], &payload);
    assert_eq!(pumped.stdout, payload);
    assert_eq!(pumped, run_direct(&["echo"], &payload));
}

#[test]
fn pump_reports_a_nonzero_exit_code() {
    let pumped = run_through_pump(&["err:boom", "exit:7"], b"");
    assert_eq!(pumped.exit, (7, 0));
    assert_eq!(pumped.stderr, b"boom");
}

/// Drive the pump through a **bounded** `SyncSender` (capacity 1 — maximum
/// backpressure) with a slow, concurrent consumer, and prove no bytes are lost.
/// A drop-on-overflow sink would lose frames under this burst; a bounded-blocking
/// sink stalls the pump (→ the child's pipe fills → the child blocks) and
/// preserves every byte. This is the soldr#2365 "bounded per-channel memory +
/// byte-exact" invariant, exercised on the pump's real output path.
#[test]
fn bounded_sink_backpressures_without_dropping_bytes() {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    // Many small writes on both streams → many frames through a depth-1 channel.
    let script = &[
        "out:A", "err:1", "out:B", "err:2", "out:C", "err:3", "out:D", "exit:0",
    ];
    let child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture under pump");

    let (stdin_tx, stdin_rx) = channel();
    stdin_tx
        .send(SessionFrame {
            kind: Some(session_frame::Kind::StdinEof(true)),
        })
        .unwrap();
    drop(stdin_tx);

    // Depth-1 bounded sink: the pump blocks whenever a frame is unconsumed.
    let (out_tx, out_rx) = sync_channel::<SessionFrame>(1);

    // Concurrent consumer that drains slowly, so the depth-1 bound is
    // continuously saturated and the pump is forced to block.
    let consumer = std::thread::spawn(move || {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_frame = None;
        for frame in out_rx {
            std::thread::sleep(Duration::from_millis(1));
            match frame.kind {
                Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
                Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
                Some(session_frame::Kind::Exit(e)) => exit_frame = Some((e.code, e.signal)),
                _ => panic!("unexpected inbound-only frame on the outbound lane"),
            }
        }
        (
            stdout,
            stderr,
            exit_frame.expect("a terminal Exit frame is always sent"),
        )
    });

    let exit = run_child_session(child, out_tx, stdin_rx).expect("pump reaps child");
    let (stdout, stderr, exit_frame) = consumer.join().expect("consumer thread");

    // No drops: every byte on each stream survives, in order, under a depth-1
    // bound. (stderr/stdout interleaving order across streams is not asserted —
    // only per-stream fidelity, which is the contract.)
    assert_eq!(stdout, b"ABCD");
    assert_eq!(stderr, b"123");
    assert_eq!(exit_frame, (exit.code, exit.signal));
    assert_eq!(exit_frame, (0, 0));
}

/// Drive the pump, then push both directions through the SESSION codec's byte
/// boundary (slice 3a). Inbound stdin is encoded → bytes → decoded before it
/// reaches the pump; every outbound frame is encoded into one concatenated
/// stream that is then decoded **one byte at a time**, so every partial-frame
/// cut is exercised on a real byte boundary. The reassembled result must equal
/// the direct oracle exactly — the codec adds framing, never touches fidelity.
fn run_through_pump_over_codec(directives: &[&str], stdin: &[u8]) -> Reassembled {
    use crate::broker::session_codec::{
        encode_session_frame, try_decode_session_frame, DecodedSessionFrame,
    };

    let child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture under pump");

    // Feed inbound stdin frames through the codec (encode → decode) before the
    // pump sees them, proving the client→daemon direction crosses the boundary.
    let (stdin_tx, stdin_rx) = channel();
    let mut seq = 0u64;
    let mut send_via_codec = |frame: SessionFrame| {
        let wire = encode_session_frame(&frame, seq).expect("encode stdin frame");
        seq += 1;
        let decoded = try_decode_session_frame(&wire)
            .expect("decode ok")
            .expect("complete")
            .frame;
        stdin_tx.send(decoded).unwrap();
    };
    if !stdin.is_empty() {
        send_via_codec(SessionFrame {
            kind: Some(session_frame::Kind::Stdin(stdin.to_vec())),
        });
    }
    send_via_codec(SessionFrame {
        kind: Some(session_frame::Kind::StdinEof(true)),
    });
    drop(stdin_tx);

    let (out_tx, out_rx) = channel();
    let exit = run_child_session(child, out_tx, stdin_rx).expect("pump reaps child");

    // Encode every outbound frame into one concatenated byte stream.
    let mut stream = Vec::new();
    for (oseq, frame) in out_rx.into_iter().enumerate() {
        stream.extend_from_slice(&encode_session_frame(&frame, oseq as u64).expect("encode out"));
    }

    // Decode the stream one byte at a time to hit every partial-frame boundary.
    let mut fed = Vec::new();
    let mut cursor = 0usize;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_frame = None;
    for &byte in &stream {
        fed.push(byte);
        while let Some(DecodedSessionFrame { frame, consumed }) =
            try_decode_session_frame(&fed[cursor..]).expect("decode ok")
        {
            match frame.kind {
                Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
                Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
                Some(session_frame::Kind::Exit(e)) => exit_frame = Some((e.code, e.signal)),
                other => panic!("unexpected frame on outbound stream: {other:?}"),
            }
            cursor += consumed;
        }
    }
    assert_eq!(
        cursor,
        stream.len(),
        "the whole outbound stream is consumed"
    );
    let exit_frame = exit_frame.expect("a terminal Exit frame is always sent");
    assert_eq!(exit_frame, (exit.code, exit.signal));
    Reassembled {
        stdout,
        stderr,
        exit: exit_frame,
    }
}

#[test]
fn pump_over_codec_matches_the_direct_oracle() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(
        run_through_pump_over_codec(script, b""),
        run_direct(script, b"")
    );
}

#[test]
fn pump_over_codec_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let got = run_through_pump_over_codec(script, b"");
    assert_eq!(got.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(got.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(got, run_direct(script, b""));
}

#[test]
fn pump_over_codec_delivers_full_byte_range_stdin() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let got = run_through_pump_over_codec(&["echo"], &payload);
    assert_eq!(got.stdout, payload);
    assert_eq!(got, run_direct(&["echo"], &payload));
}
