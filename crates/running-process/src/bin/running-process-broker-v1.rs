//! Entry point for the v1 broker daemon.
//!
//! Phase 4 lands this binary incrementally. This first slice exposes a
//! real binary target and wires the compiled crate surface; the socket
//! accept loop and admin verbs land in follow-up PRs under #235.

fn main() {
    let mut args = std::env::args();
    let program = args
        .next()
        .unwrap_or_else(|| "running-process-broker-v1".to_string());
    match args.next().as_deref() {
        Some("--version") | Some("-V") => {
            println!("running-process-broker-v1 {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => {
            print_help(&program);
        }
        None => {
            eprintln!("running-process-broker-v1 serve mode is not implemented yet; see #235");
            std::process::exit(2);
        }
        Some(other) => {
            eprintln!("unsupported argument {other:?}");
            print_help(&program);
            std::process::exit(2);
        }
    }
}

fn print_help(program: &str) {
    println!("{program} [--help] [--version]");
    println!();
    println!("Phase 4 broker daemon entry point. Serve mode lands in #235 follow-up PRs.");
}
