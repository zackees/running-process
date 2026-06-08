//! Entry point for the v1 broker daemon.
//!
//! Phase 4 lands this binary incrementally. It supports local admin renderers,
//! single-connection Hello tests, and a bounded serve mode for an already
//! registered backend endpoint.

use running_process::broker::server::admin::{
    render_backend_health_json, render_config_json, render_diagnose_json, render_dump_json,
    render_healthz, render_list_instances_json, render_metrics_text, render_status_json,
    AdminSnapshot,
};
use running_process::broker::server::{
    serve_launching_backends, serve_one_local_socket, serve_registered_backend,
    BrokerLaunchServeConfig, BrokerServeConfig, HelloHandler,
};
use running_process::broker::{
    client::send_admin_request,
    lifecycle::refuse_privileged_run,
    protocol::{AdminReply, AdminRequest, AdminVerb},
};

const ADMIN_SOCKET_ENV: &str = "RUNNING_PROCESS_BROKER_V1_SOCKET";

fn main() {
    if let Err(err) = refuse_privileged_run() {
        eprintln!("{err}");
        std::process::exit(1);
    }

    let mut args = std::env::args();
    let program = args
        .next()
        .unwrap_or_else(|| "running-process-broker-v1".to_string());
    let snapshot = AdminSnapshot::local_not_serving();
    let (rest, cli_admin_socket) =
        parse_global_admin_socket(args.collect()).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(2);
        });
    let admin_socket = cli_admin_socket.or_else(|| {
        std::env::var(ADMIN_SOCKET_ENV)
            .ok()
            .filter(|value| !value.is_empty())
    });
    match rest.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("running-process-broker-v1 {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => {
            print_help(&program);
        }
        Some("status") => {
            let command = AdminCommand {
                verb: AdminVerb::Status,
                json: has_flag(&rest[1..], "--json"),
                service_name: String::new(),
                output_path: String::new(),
            };
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            if command.json {
                println!("{}", render_status_json(&snapshot));
            } else {
                print_admin_reply(render_local_admin_reply(&snapshot, command));
            }
        }
        Some("dump") => {
            let command = AdminCommand::json(AdminVerb::Dump);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            println!("{}", render_dump_json(&snapshot));
        }
        Some("list-instances") => {
            let command = AdminCommand::json(AdminVerb::ListInstances);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            println!("{}", render_list_instances_json(&snapshot));
        }
        Some("healthz") => {
            let command = AdminCommand::text(AdminVerb::Healthz);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            print!("{}", render_healthz());
        }
        Some("readyz") => {
            let command = AdminCommand::text(AdminVerb::Readyz);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            print_admin_reply(render_local_admin_reply(&snapshot, command));
        }
        Some("backend-health") => {
            let service = first_positional(&rest[1..]).unwrap_or("unknown");
            let command = AdminCommand {
                verb: AdminVerb::BackendHealth,
                json: true,
                service_name: service.into(),
                output_path: String::new(),
            };
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            println!("{}", render_backend_health_json(&snapshot, service));
        }
        Some("config") => {
            let command = AdminCommand::json(AdminVerb::Config);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            println!("{}", render_config_json(&snapshot));
        }
        Some("diagnose") => {
            let output = option_value(&rest[1..], "--output").unwrap_or("bundle.tar.gz");
            let command = AdminCommand {
                verb: AdminVerb::Diagnose,
                json: true,
                service_name: String::new(),
                output_path: output.into(),
            };
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
            println!("{}", render_diagnose_json(&snapshot, output));
        }
        Some("metrics") => {
            let command = AdminCommand::text(AdminVerb::Metrics);
            if let Some(endpoint) = admin_socket.as_deref() {
                run_live_admin_command(endpoint, command);
            }
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
        Some("--serve-launch") => {
            let Some(socket_path) = rest.get(1) else {
                eprintln!("--serve-launch requires a socket path or pipe name");
                std::process::exit(2);
            };
            let max_connections = option_value(&rest[2..], "--max-connections")
                .map(parse_connection_count)
                .unwrap_or(Ok(1))
                .unwrap_or_else(|err| {
                    eprintln!("{err}");
                    std::process::exit(2);
                });

            let mut config = BrokerLaunchServeConfig::new(socket_path, max_connections)
                .unwrap_or_else(|err| {
                    eprintln!("invalid serve-launch config: {err}");
                    std::process::exit(2);
                });
            if let Some(root) = option_value(&rest[2..], "--service-def-dir") {
                config = config.with_service_definition_dir(root);
            }

            if let Err(err) = serve_launching_backends(config) {
                eprintln!("serve-launch failed: {err}");
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
    println!("{program} [--socket <endpoint>] status [--json]");
    println!("{program} [--socket <endpoint>] dump --json");
    println!("{program} [--socket <endpoint>] list-instances --json");
    println!("{program} [--socket <endpoint>] healthz");
    println!("{program} [--socket <endpoint>] readyz");
    println!("{program} [--socket <endpoint>] backend-health <service> --json");
    println!("{program} [--socket <endpoint>] config --effective --json");
    println!("{program} [--socket <endpoint>] diagnose --output <bundle.tar.gz>");
    println!("{program} [--socket <endpoint>] metrics");
    println!("{program} --serve-once <socket-path-or-pipe-name>");
    println!(
        "{program} --serve <socket-path-or-pipe-name> --service <name> --version <semver> --backend-endpoint <path> [--service-def-dir <dir>] [--max-connections <n>]"
    );
    println!(
        "{program} --serve-launch <socket-path-or-pipe-name> [--service-def-dir <dir>] [--max-connections <n>]"
    );
    println!();
    println!("Admin commands use --socket, or {ADMIN_SOCKET_ENV}, to query a running broker.");
    println!("Phase 4 broker daemon entry point. Serve mode is bounded until the long-lived spawn coordinator lands.");
}

#[derive(Clone)]
struct AdminCommand {
    verb: AdminVerb,
    json: bool,
    service_name: String,
    output_path: String,
}

impl AdminCommand {
    fn json(verb: AdminVerb) -> Self {
        Self {
            verb,
            json: true,
            service_name: String::new(),
            output_path: String::new(),
        }
    }

    fn text(verb: AdminVerb) -> Self {
        Self {
            verb,
            json: false,
            service_name: String::new(),
            output_path: String::new(),
        }
    }

    fn request(self) -> AdminRequest {
        AdminRequest {
            verb: self.verb as i32,
            json: self.json,
            service_name: self.service_name,
            output_path: self.output_path,
        }
    }
}

fn render_local_admin_reply(snapshot: &AdminSnapshot, command: AdminCommand) -> AdminReply {
    running_process::broker::server::admin::render_admin_reply(snapshot, &command.request())
}

fn run_live_admin_command(endpoint: &str, command: AdminCommand) -> ! {
    match send_admin_request(endpoint, command.request()) {
        Ok(reply) => {
            print_admin_reply(reply);
        }
        Err(err) => {
            eprintln!("admin request to {endpoint:?} failed: {err}");
            std::process::exit(1);
        }
    }
}

fn print_admin_reply(reply: AdminReply) -> ! {
    print!("{}", reply.body);
    if !reply.body.ends_with('\n') {
        println!();
    }
    let exit_code = i32::try_from(reply.exit_code).unwrap_or(1);
    std::process::exit(exit_code);
}

fn parse_global_admin_socket(args: Vec<String>) -> Result<(Vec<String>, Option<String>), String> {
    let mut rest = Vec::with_capacity(args.len());
    let mut socket = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--socket" {
            let Some(value) = iter.next() else {
                return Err("--socket requires a socket path or pipe name".into());
            };
            if value.is_empty() {
                return Err("--socket requires a non-empty socket path or pipe name".into());
            }
            if socket.replace(value).is_some() {
                return Err("--socket may only be provided once".into());
            }
        } else {
            rest.push(arg);
        }
    }
    Ok((rest, socket))
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
