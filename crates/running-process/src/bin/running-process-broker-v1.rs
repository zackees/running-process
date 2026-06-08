//! Entry point for the v1 broker daemon.
//!
//! Phase 4 lands this binary incrementally. It supports local admin renderers,
//! single-connection Hello tests, and a bounded serve mode for an already
//! registered backend endpoint.

use running_process::broker::server::admin::{
    render_backend_health_json, render_config_json, render_diagnose_json, render_dump_json,
    render_healthz, render_list_instances_json, render_metrics_text, render_readyz,
    render_status_json, AdminSnapshot,
};
use running_process::broker::server::{
    serve_one_local_socket, serve_registered_backend, BrokerServeConfig, HelloHandler,
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
        Some("--serve-once") => {
            let Some(socket_path) = rest.get(1) else {
                eprintln!("--serve-once requires a socket path or pipe name");
                std::process::exit(2);
            };
            let handler = HelloHandler::new();
            if let Err(err) = serve_one_local_socket(socket_path, &handler) {
                eprintln!("serve-once failed: {err}");
                std::process::exit(1);
            }
        }
        Some("--serve") => {
            let Some(socket_path) = rest.get(1) else {
                eprintln!("--serve requires a socket path or pipe name");
                std::process::exit(2);
            };
            let service_name = required_option(&rest[2..], "--service");
            let service_version = required_option(&rest[2..], "--version");
            let backend_endpoint = required_option(&rest[2..], "--backend-endpoint");
            let max_connections = option_value(&rest[2..], "--max-connections")
                .map(parse_connection_count)
                .unwrap_or(Ok(1))
                .unwrap_or_else(|err| {
                    eprintln!("{err}");
                    std::process::exit(2);
                });

            let mut config = BrokerServeConfig::new(
                socket_path,
                service_name,
                service_version,
                backend_endpoint,
                max_connections,
            )
            .unwrap_or_else(|err| {
                eprintln!("invalid serve config: {err}");
                std::process::exit(2);
            });
            if let Some(root) = option_value(&rest[2..], "--service-def-dir") {
                config = config.with_service_definition_dir(root);
            }

            if let Err(err) = serve_registered_backend(config) {
                eprintln!("serve failed: {err}");
                std::process::exit(1);
            }
        }
        None => {
            eprintln!("no broker command provided");
            print_help(&program);
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
    println!("{program} --serve-once <socket-path-or-pipe-name>");
    println!(
        "{program} --serve <socket-path-or-pipe-name> --service <name> --version <semver> --backend-endpoint <path> [--service-def-dir <dir>] [--max-connections <n>]"
    );
    println!();
    println!("Phase 4 broker daemon entry point. Serve mode is bounded until the long-lived spawn coordinator lands.");
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

fn required_option<'a>(args: &'a [String], option: &str) -> &'a str {
    option_value(args, option).unwrap_or_else(|| {
        eprintln!("{option} is required for --serve");
        std::process::exit(2);
    })
}

fn parse_connection_count(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("--max-connections must be a positive integer, got {value:?}"))?;
    if parsed == 0 {
        return Err("--max-connections must be greater than zero".into());
    }
    Ok(parsed)
}
