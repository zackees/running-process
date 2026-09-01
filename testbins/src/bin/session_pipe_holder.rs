//! Session fixture: leave a grandchild holding the direct parent's stdio.
//!
//! The direct process exits immediately after reporting the grandchild PID.
//! Its child inherits stdout/stderr, so a process session must observe the
//! direct exit without treating the still-open pipe as proof that it lives.

use std::io::Write;
use std::process::Command;

fn main() {
    let target = std::env::args()
        .nth(1)
        .expect("session_pipe_holder requires a sleeper path");
    let child = Command::new(target)
        .spawn()
        .expect("spawn pipe-holding grandchild");
    println!("GRANDCHILD_PID={}", child.id());
    std::io::stdout().flush().expect("flush grandchild pid");
    // Give the session pumps a deterministic opportunity to observe the
    // direct parent's output before that parent exits and the grandchild is
    // solely responsible for keeping the pipe open.
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(child);
}
