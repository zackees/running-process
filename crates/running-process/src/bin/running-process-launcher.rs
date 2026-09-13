//! Scheduler-owned helper. Only the IPC endpoint appears in command metadata.

fn main() {
    let mut args = std::env::args_os().skip(1);
    let endpoint = args.next();
    let result = match endpoint.as_ref().and_then(|value| value.to_str()) {
        Some(endpoint) if args.next().is_none() => {
            running_process::independent_spawn::run_launcher(endpoint)
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected one launcher endpoint",
        )),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            // Do not echo payloads, endpoint names, or host error messages.
            eprintln!("independent launcher failed ({:?})", error.kind());
            std::process::exit(1);
        }
    }
}
