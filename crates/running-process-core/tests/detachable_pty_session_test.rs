use running_process_core::pty::{DetachablePtySession, NativePtyProcess};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn sleeping_session() -> DetachablePtySession {
    let process = NativePtyProcess::new(sleep_command(), None, None, 24, 80, None)
        .expect("create PTY process");
    DetachablePtySession::spawn(process).expect("spawn detachable PTY")
}

fn sleep_command() -> Vec<String> {
    vec![testbin_path("testbin-sleeper")
        .to_string_lossy()
        .into_owned()]
}

fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", name, "--message-format=json"])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("failed to run cargo build");
    assert!(
        output.status.success(),
        "`cargo build -p {name}` failed with status {}",
        output.status,
    );

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
                    assert!(p.exists(), "cargo reported {p:?} but it does not exist");
                    return p;
                }
            }
        }
    }

    panic!("`cargo build -p {name}` succeeded but no binary artifact found in JSON output");
}

#[test]
fn detach_keeps_child_alive_and_later_reattach_can_write() {
    let session = sleeping_session();

    let first = session.attach().expect("first attach");
    first.write(b"one\n", true).expect("write first line");
    first.detach();
    let after_first_write = session.process().pty_input_bytes_total();

    assert!(!session.is_attached());
    assert!(
        session.wait(Some(0.1)).is_err(),
        "child should still be running after detach"
    );
    assert!(
        after_first_write >= 4,
        "first attachment write should be recorded"
    );

    let second = session.attach().expect("second attach");
    second.write(b"two\n", true).expect("write second line");
    second.detach();

    assert!(
        session.process().pty_input_bytes_total() > after_first_write,
        "second attachment write should be recorded after reattach"
    );
    session.terminate_tree().expect("terminate tree");
    assert!(session.wait(Some(5.0)).is_ok());
}

#[test]
fn only_one_attachment_is_active_at_a_time() {
    let session = sleeping_session();

    let first = session.attach().expect("first attach");
    assert!(session.attach().is_err(), "second attach must be rejected");
    drop(first);

    assert!(
        session.attach().is_ok(),
        "dropping attachment should detach it"
    );
    session.terminate_tree().expect("terminate tree");
    assert!(session.wait(Some(5.0)).is_ok());
}

#[test]
fn cloned_session_handles_share_the_attachment_gate() {
    let session = sleeping_session();
    let sibling = session.clone();

    let attachment = session.attach().expect("attach original");
    assert!(
        sibling.attach().is_err(),
        "cloned handles should reject concurrent attachments"
    );
    attachment.detach();

    let sibling_attachment = sibling.attach().expect("attach sibling");
    sibling_attachment.detach();

    session.terminate_tree().expect("terminate tree");
    assert!(sibling.wait(Some(5.0)).is_ok());
}

#[test]
fn terminate_tree_remains_separate_from_detach() {
    let session = sleeping_session();
    let attachment = session.attach().expect("attach");
    attachment.detach();

    assert!(
        session.wait(Some(0.1)).is_err(),
        "detach should not terminate the child"
    );
    session.terminate_tree().expect("terminate tree");
    assert!(
        session.wait(Some(5.0)).is_ok(),
        "terminate_tree should reap the child"
    );
}
