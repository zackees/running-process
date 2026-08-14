//! Test binary: prints its PID, originator, and requested environment keys,
//! then sleeps so daemon session tests can attach to its backlog.

use std::io::Write;

fn main() {
    let pid = std::process::id();
    println!("PID={pid}");
    match std::env::var(running_process::ORIGINATOR_ENV_VAR) {
        Ok(val) => println!("ORIGINATOR={val}"),
        Err(_) => println!("ORIGINATOR=<not set>"),
    }
    for key in std::env::args().skip(1) {
        match std::env::var(&key) {
            Ok(value) => println!("ENV:{key}={value}"),
            Err(_) => println!("ENV:{key}=<unset>"),
        }
    }
    println!("READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
