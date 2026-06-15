//! Backpressure test fixture for #449.
//!
//! Reads stdin in 4 KB chunks but sleeps 20 ms between reads. Used
//! to verify the host's master input write path tolerates a slow
//! consumer without truncating large pastes. Echoes everything to
//! stdout (also in 4 KB writes + flush) so tests can assert
//! byte-equal arrival of multi-MB payloads.

use std::io::{Read, Write};
use std::time::Duration;

/// On POSIX hosts the default PTY line discipline cooks input bytes
/// (echoes, renders control characters as `^X`, and translates CR/LF).
/// All three would defeat the byte-exact paste tests in #449. Apply
/// `cfmakeraw` to stdin so the line discipline is removed entirely on
/// POSIX. Windows ConPTY in PASSTHROUGH_MODE handles this for us.
#[cfg(unix)]
fn enter_raw_mode() {
    use libc::{cfmakeraw, tcgetattr, tcsetattr, termios, TCSANOW};
    unsafe {
        let fd = 0; // STDIN_FILENO
        let mut t: termios = std::mem::zeroed();
        if tcgetattr(fd, &mut t) != 0 {
            return;
        }
        cfmakeraw(&mut t);
        let _ = tcsetattr(fd, TCSANOW, &t);
    }
}

#[cfg(not(unix))]
fn enter_raw_mode() {}

fn main() {
    enter_raw_mode();

    let mut sleep_ms: u64 = 20;
    let mut buf_size: usize = 4096;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sleep-ms" => {
                sleep_ms = args
                    .next()
                    .expect("--sleep-ms requires an integer")
                    .parse()
                    .expect("--sleep-ms argument must be an integer");
            }
            "--buf-size" => {
                buf_size = args
                    .next()
                    .expect("--buf-size requires an integer")
                    .parse()
                    .expect("--buf-size argument must be an integer");
            }
            other => {
                eprintln!("slow_stdin_reader: unknown flag {other}");
                std::process::exit(2);
            }
        }
    }

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    let mut buf = vec![0u8; buf_size];
    let interval = Duration::from_millis(sleep_ms);
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                stdout.write_all(&buf[..n]).expect("echo write");
                stdout.flush().expect("echo flush");
                std::thread::sleep(interval);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("slow_stdin_reader: read error: {e}");
                std::process::exit(1);
            }
        }
    }
}
