//! Test binary: spawns N sleeper children, prints all PIDs, and sleeps forever.
//! Usage: spawner <count> <sleeper_path>
//!
//! Output format (one line per child, then a READY marker):
//!   CHILD_PID=<pid>
//!   READY
//!
//! Used by containment integration tests to verify grandchild cleanup.

use std::io::Write;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: spawner <count> <sleeper_path>");
        std::process::exit(1);
    }
    let count: usize = args[1].parse().expect("count must be a number");
    let sleeper_path = &args[2];

    println!("SPAWNER_PID={}", std::process::id());
    std::io::stdout().flush().unwrap();

    let mut children = Vec::new();
    for _ in 0..count {
        let child = Command::new(sleeper_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn sleeper");
        let pid = child.id();
        println!("CHILD_PID={pid}");
        std::io::stdout().flush().unwrap();
        children.push(child);
    }

    println!("READY");
    std::io::stdout().flush().unwrap();

    // Sleep forever — expect to be killed by the test harness.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
