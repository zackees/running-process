use super::*;

fn command(script: &str) -> Vec<String> {
    if std::env::consts::OS == "windows" {
        vec!["cmd.exe".into(), "/C".into(), script.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
}

fn sleeper() -> Vec<String> {
    if std::env::consts::OS == "windows" {
        command("ping -n 30 127.0.0.1 >NUL")
    } else {
        command("sleep 30")
    }
}

fn emitter() -> Vec<String> {
    if std::env::consts::OS == "windows" {
        command("echo interactive-coverage")
    } else {
        command("printf interactive-coverage")
    }
}

#[test]
fn interactive_session_pumps_resizes_waits_and_closes() {
    let process = NativePtyProcess::new(emitter(), None, None, 24, 80, None).unwrap();
    let session = InteractivePtySession::with_options(
        process,
        InteractivePtyOptions {
            echo_output: false,
            relay_terminal_input: false,
            respond_to_queries: false,
        },
    );
    let default_session = InteractivePtySession::new(
        NativePtyProcess::new(emitter(), None, None, 24, 80, None).unwrap(),
    );
    assert!(default_session.process().pid().unwrap().is_none());
    drop(default_session);
    assert!(session.process().pid().unwrap().is_none());
    let _ = session.resize(30, 100);
    let _ = session.send_interrupt();
    session.start().unwrap();
    let _ = session.pump_output(Some(5.0), false).unwrap();
    let _ = session.resize(30, 100);
    let _ = session.wait_and_drain(Some(0.1), 0.1);
    session.close().unwrap();
    let closed = session.pump_output(Some(0.1), true).unwrap();
    assert!(closed.stream_closed || !closed.chunks.is_empty());
    session.close().unwrap();
}

#[test]
fn interactive_options_relay_and_termination_methods_are_live() {
    let terminate = NativePtyProcess::new(sleeper(), None, None, 24, 80, None).unwrap();
    let terminate = InteractivePtySession::with_options(
        terminate,
        InteractivePtyOptions {
            echo_output: false,
            relay_terminal_input: std::env::consts::OS != "windows",
            respond_to_queries: false,
        },
    );
    terminate.start().unwrap();
    terminate.terminate().unwrap();
    let _ = terminate.wait(Some(5.0)).unwrap();
    terminate.close().unwrap();

    let kill = NativePtyProcess::new(sleeper(), None, None, 24, 80, None).unwrap();
    let kill = InteractivePtySession::with_options(
        kill,
        InteractivePtyOptions {
            echo_output: false,
            relay_terminal_input: false,
            respond_to_queries: false,
        },
    );
    kill.start().unwrap();
    kill.kill().unwrap();
    let _ = kill.wait(Some(5.0)).unwrap();
    kill.close().unwrap();
}

#[test]
fn blocking_reader_waits_cover_unbounded_notification_paths() {
    let process = Arc::new(NativePtyProcess::new(emitter(), None, None, 24, 80, None).unwrap());
    process.start_impl().unwrap();
    let reader = Arc::clone(&process);
    let read = thread::spawn(move || reader.read_chunk_impl(None));
    match read.join().unwrap() {
        Ok(Some(_)) => {}
        Err(PtyError::Other(message)) => {
            assert_eq!(message, "Pseudo-terminal stream is closed")
        }
        outcome => panic!("unexpected unbounded PTY read result: {outcome:?}"),
    }
    let _ = process.kill_impl();
    let _ = process.wait_impl(Some(10.0));
    process.mark_reader_closed();
    assert!(process.wait_for_reader_closed_impl(None));
    process.close_impl().unwrap();
}
