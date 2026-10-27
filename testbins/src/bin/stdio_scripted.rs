//! Deterministic, byte-exact stdio fixture for the Phase 3 stdio-fidelity
//! oracle (soldr#2365). Replays a script given as argv directives so a test
//! can assert that a proxy path reproduces this exact stdout/stderr/exit
//! behavior against direct execution as the oracle.
//!
//! Directives (applied in argv order):
//! - `out:<utf8>`      — write the UTF-8 text to stdout (no trailing newline)
//! - `err:<utf8>`      — write the UTF-8 text to stderr
//! - `outhex:<hex>`    — write the hex-decoded raw bytes to stdout
//! - `outrep:<hex>:<n>` — write the hex-decoded bytes `n` times to stdout.
//!   Same output as a long `outhex:`, without putting the payload on the
//!   command line: Windows caps a command line at 32,767 characters, and a
//!   hex-encoded argument is twice the size of the bytes it carries.
//! - `errhex:<hex>`    — write the hex-decoded raw bytes to stderr
//! - `env:<name>`      — write one environment value to stdout
//! - `cwd`             — write the current working directory to stdout
//! - `flush`           — flush stdout then stderr (control interleaving)
//! - `echo`           — copy all of stdin to stdout verbatim, then flush
//! - `sleep-ms:<n>`   — sleep for `<n>` milliseconds
//! - `exit:<code>`     — exit immediately with `<code>`
//!
//! With no `exit:` directive the process exits 0 after the last directive.
//! Everything is flushed as it is written so interleaving is caller-controlled
//! (rustc emits artifacts on stdout and JSON diagnostics on stderr; the oracle
//! must prove the two streams stay independent and byte-exact).

use std::io::{Read, Write};
use std::process::exit;

fn decode_hex(hex: &str) -> Vec<u8> {
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).expect("hex digit");
        let lo = (bytes[i + 1] as char).to_digit(16).expect("hex digit");
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    out
}

fn repeated_hex(directive: &str) -> (usize, Vec<u8>) {
    let (count, hex) = directive
        .split_once(':')
        .expect("repeat directive is count:hex");
    (
        count.parse().expect("repeat count is an integer"),
        decode_hex(hex),
    )
}

fn main() {
    #[cfg(windows)]
    if report_wide_argument() {
        return;
    }
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();

    for arg in std::env::args().skip(1) {
        if let Some(text) = arg.strip_prefix("out:") {
            stdout.write_all(text.as_bytes()).expect("write stdout");
            stdout.flush().expect("flush stdout");
        } else if let Some(text) = arg.strip_prefix("err:") {
            stderr.write_all(text.as_bytes()).expect("write stderr");
            stderr.flush().expect("flush stderr");
        } else if let Some(spec) = arg.strip_prefix("outrep:") {
            let (hex, count) = spec.rsplit_once(':').expect("outrep:<hex>:<count>");
            let unit = decode_hex(hex);
            let count: usize = count.parse().expect("outrep count");
            for _ in 0..count {
                stdout.write_all(&unit).expect("write stdout repeat");
            }
            stdout.flush().expect("flush stdout");
        } else if let Some(hex) = arg.strip_prefix("outhex:") {
            stdout
                .write_all(&decode_hex(hex))
                .expect("write stdout hex");
            stdout.flush().expect("flush stdout");
        } else if let Some(hex) = arg.strip_prefix("errhex:") {
            stderr
                .write_all(&decode_hex(hex))
                .expect("write stderr hex");
            stderr.flush().expect("flush stderr");
        } else if let Some(repeat) = arg.strip_prefix("repeat-outhex:") {
            let (count, bytes) = repeated_hex(repeat);
            for _ in 0..count {
                stdout.write_all(&bytes).expect("write repeated stdout hex");
            }
            stdout.flush().expect("flush stdout");
        } else if let Some(repeat) = arg.strip_prefix("repeat-errhex:") {
            let (count, bytes) = repeated_hex(repeat);
            for _ in 0..count {
                stderr.write_all(&bytes).expect("write repeated stderr hex");
            }
            stderr.flush().expect("flush stderr");
        } else if let Some(name) = arg.strip_prefix("env:") {
            let value = std::env::var(name).expect("fixture environment variable");
            stdout
                .write_all(value.as_bytes())
                .expect("write environment");
        } else if arg == "cwd" {
            let cwd = std::env::current_dir().expect("fixture cwd");
            stdout
                .write_all(cwd.to_string_lossy().as_bytes())
                .expect("write cwd");
        } else if arg == "flush" {
            stdout.flush().expect("flush stdout");
            stderr.flush().expect("flush stderr");
        } else if arg == "echo" {
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .expect("read stdin");
            stdout.write_all(&input).expect("echo stdin to stdout");
            stdout.flush().expect("flush stdout");
        } else if let Some(milliseconds) = arg.strip_prefix("sleep-ms:") {
            std::thread::sleep(std::time::Duration::from_millis(
                milliseconds
                    .parse::<u64>()
                    .expect("sleep duration is an integer"),
            ));
        } else if let Some(code) = arg.strip_prefix("exit:") {
            stdout.flush().ok();
            stderr.flush().ok();
            exit(code.parse::<i32>().expect("exit code is an integer"));
        } else {
            eprintln!("stdio_scripted: unknown directive: {arg}");
            exit(64); // EX_USAGE
        }
    }

    stdout.flush().ok();
    stderr.flush().ok();
}

#[cfg(windows)]
fn report_wide_argument() -> bool {
    use std::os::windows::ffi::OsStrExt;

    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("report-wide")) {
        return false;
    }
    let arg = args.next().expect("wide argument");
    let bytes: Vec<u8> = arg.encode_wide().flat_map(u16::to_le_bytes).collect();
    std::io::stdout()
        .write_all(&bytes)
        .expect("write wide argument");
    true
}
