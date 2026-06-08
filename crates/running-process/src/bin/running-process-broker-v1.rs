//! Entry point for the v1 broker daemon.
//!
//! Phase 4 lands this binary incrementally. This first slice exposes a
//! real binary target and wires the compiled crate surface. The socket
//! accept loop lands in follow-up PRs under #235.

use running_process::broker::server::admin::{
    render_backend_health_json, render_config_json, render_diagnose_json, render_dump_json,
    render_healthz, render_list_instances_json, render_metrics_text, render_readyz,
    render_status_json, AdminSnapshot,
};

fn main() {
    let mut args = std::env::args();
    let program = args
        .next()
        .unwrap_or_else(|| "running-process-broker-v1".to_string());
    let snapshot = AdminSnapshot::local_not_serving();
    let rest: Vec<String> = args.collect();
    match rest.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("running-process-broker-v1 {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => {
            print_help(&program);
        }
        Some("status") => {
            if has_flag(&rest[1..], "--json") {
                println!("{}", render_status_json(&snapshot));
            } else {
                println!("broker_instance: {}", snapshot.broker_instance);
                println!("accepting_hello: {}", snapshot.accepting_hello);
            }
        }
        Some("dump") => {
            println!("{}", render_dump_json(&snapshot));
        }
        Some("list-instances") => {
            println!("{}", render_list_instances_json(&snapshot));
        }
        Some("healthz") => {
            print!("{}", render_healthz());
        }
        Some("readyz") => {
            print!("{}", render_readyz(&snapshot));
            if !snapshot.accepting_hello {
                std::process::exit(1);
            }
        }
        Some("backend-health") => {
            let service = first_positional(&rest[1..]).unwrap_or("unknown");
            println!("{}", render_backend_health_json(&snapshot, service));
        }
        Some("config") => {
            println!("{}", render_config_json(&snapshot));
        }
        Some("diagnose") => {
            let output = option_value(&rest[1..], "--output").unwrap_or("bundle.tar.gz");
            println!("{}", render_diagnose_json(&snapshot, output));
        }
        Some("metrics") => {
            print!("{}", render_metrics_text(&snapshot));
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
    println!("{program} status [--json]");
    println!("{program} dump --json");
    println!("{program} list-instances --json");
    println!("{program} healthz");
    println!("{program} readyz");
    println!("{program} backend-health <service> --json");
    println!("{program} config --effective --json");
    println!("{program} diagnose --output <bundle.tar.gz>");
    println!("{program} metrics");
    println!();
    println!("Phase 4 broker daemon entry point. Serve mode lands in #235 follow-up PRs.");
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn first_positional(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
}

fn option_value<'a>(args: &'a [String], option: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == option)
        .map(|window| window[1].as_str())
}
