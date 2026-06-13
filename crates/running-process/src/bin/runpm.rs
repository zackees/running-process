// runpm — PM2-style process supervisor CLI (Phase 1: skeleton).
// Daemon stubs respond OK; real lifecycle lands in Phase 2. See issue #106.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use running_process::client::{connect_or_start, ClientError, DaemonClient};
use running_process::maintenance::run_release_handles;
use running_process::proto::daemon::{DaemonResponse, ServiceConfig, ServiceState, StatusCode};

#[derive(Parser)]
#[command(
    name = "runpm",
    about = "PM2-style process supervisor for running-process. See https://github.com/zackees/running-process/issues/106"
)]
struct Cli {
    /// Spawn the daemon detached and exit
    #[arg(long, global = true)]
    start_daemon: bool,

    /// Stop the daemon (alias for `kill`)
    #[arg(long, global = true)]
    stop_daemon: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a supervised service
    Start {
        /// Command to run (executable plus arguments)
        #[arg(required = true, num_args = 1..)]
        cmd: Vec<String>,
        /// Service name (defaults to basename of `cmd[0]`)
        #[arg(long)]
        name: Option<String>,
        /// Working directory
        #[arg(long)]
        cwd: Option<String>,
        /// Environment variables (KEY=VALUE), repeatable
        #[arg(long = "env")]
        env: Vec<String>,
        /// Disable auto-restart on exit
        #[arg(long)]
        no_autorestart: bool,
        /// Maximum restart attempts (0 = unlimited)
        #[arg(long, default_value_t = 0u32)]
        max_restarts: u32,
    },
    /// Stop a supervised service
    Stop {
        /// Service name, id, or "all"
        target: String,
    },
    /// Restart a supervised service
    Restart {
        /// Service name, id, or "all"
        target: String,
    },
    /// Delete a service from the registry
    Delete {
        /// Service name, id, or "all"
        target: String,
    },
    /// List all supervised services
    #[command(alias = "ls", alias = "status")]
    List {
        /// Emit JSON instead of a human-readable table
        #[arg(long)]
        json: bool,
    },
    /// Show details about a single service
    #[command(alias = "describe")]
    Show {
        /// Service name or id
        target: String,
    },
    /// Show buffered logs for a service
    Logs {
        /// Service name or id (default: all)
        target: Option<String>,
        /// Number of trailing lines
        #[arg(long, default_value_t = 100u32)]
        lines: u32,
        /// Follow log output
        #[arg(long)]
        follow: bool,
    },
    /// Flush log buffers for a service
    Flush {
        /// Service name or id (default: all)
        target: Option<String>,
    },
    /// Persist the current service set to a snapshot
    Save,
    /// Restore services from the latest snapshot
    Resurrect,
    /// Install runpm to start at boot (Phase 4 stub)
    Startup,
    /// Uninstall runpm boot integration (Phase 4 stub)
    Unstartup,
    /// Ping the daemon
    Ping,
    /// Stop the running daemon
    Kill,
    /// Maintenance subcommands (#228 Phase 1)
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommands,
    },
}

#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Ask live daemons to release any open handles under `PATH`.
    ///
    /// POSIX: no-op (delete-on-close semantics make this unnecessary).
    /// Windows: Phase 1 ships a scaffold; the full handler ships in
    /// Phase 2 once the manifest registry exists. See issue #230.
    ReleaseHandles {
        /// Filesystem path that the caller wants to rm -rf.
        #[arg(long)]
        path: PathBuf,
        /// Emit a JSON document on stdout instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.start_daemon {
        return run_to_exit(cmd_start_daemon());
    }
    if cli.stop_daemon {
        return run_to_exit(cmd_kill());
    }

    let Some(command) = cli.command else {
        eprintln!("error: a subcommand is required (try `runpm --help`)");
        return ExitCode::from(2);
    };

    let result = match command {
        Commands::Start {
            cmd,
            name,
            cwd,
            env,
            no_autorestart,
            max_restarts,
        } => cmd_start(cmd, name, cwd, env, !no_autorestart, max_restarts),
        Commands::Stop { target } => cmd_simple_target("stop", &target, |c, t| c.service_stop(t)),
        Commands::Restart { target } => {
            cmd_simple_target("restart", &target, |c, t| c.service_restart(t))
        }
        Commands::Delete { target } => {
            cmd_simple_target("delete", &target, |c, t| c.service_delete(t))
        }
        Commands::List { json } => cmd_list(json),
        Commands::Show { target } => cmd_show(&target),
        Commands::Logs {
            target,
            lines,
            follow,
        } => cmd_logs(target.as_deref().unwrap_or(""), lines, follow),
        Commands::Flush { target } => {
            cmd_simple_target("flush", target.as_deref().unwrap_or("all"), |c, t| {
                c.service_flush(t)
            })
        }
        Commands::Save => cmd_no_arg("save", |c| c.service_save()),
        Commands::Resurrect => cmd_no_arg("resurrect", |c| c.service_resurrect()),
        Commands::Startup => cmd_phase4_stub("startup"),
        Commands::Unstartup => cmd_phase4_stub("unstartup"),
        Commands::Ping => cmd_ping(),
        Commands::Kill => cmd_kill(),
        Commands::Maintenance { command } => cmd_maintenance(command),
    };

    run_to_exit(result)
}

fn run_to_exit(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

fn cmd_start_daemon() -> Result<()> {
    let mut client = connect_or_start(None).context("failed to connect to or start daemon")?;
    let resp = client.ping().map_err(client_err)?;
    let server_time = resp.ping.map(|p| p.server_time_ms).unwrap_or(0);
    let status = client.status().map_err(client_err)?;
    if let Some(s) = status.status {
        println!(
            "daemon ready (server_time_ms={server_time}, socket={})",
            s.socket_path
        );
    } else {
        println!("daemon ready (server_time_ms={server_time})");
    }
    Ok(())
}

fn cmd_ping() -> Result<()> {
    let mut client = connect()?;
    let resp = client.ping().map_err(client_err)?;
    let server_time = resp.ping.map(|p| p.server_time_ms).unwrap_or(0);
    println!("OK (server_time_ms={server_time})");
    Ok(())
}

fn cmd_kill() -> Result<()> {
    let mut client = match DaemonClient::connect(None) {
        Ok(c) => c,
        Err(_) => {
            println!("daemon is not running");
            return Ok(());
        }
    };
    client.shutdown(true, 5.0).map_err(client_err)?;
    println!("daemon shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Service subcommands
// ---------------------------------------------------------------------------

fn cmd_start(
    cmd: Vec<String>,
    name: Option<String>,
    cwd: Option<String>,
    env_args: Vec<String>,
    autorestart: bool,
    max_restarts: u32,
) -> Result<()> {
    let env = parse_env(&env_args)?;
    let resolved_name = match name {
        Some(n) => n,
        None => default_name_from(&cmd[0])?,
    };

    let config = ServiceConfig {
        name: resolved_name,
        cmd,
        cwd: cwd.unwrap_or_default(),
        env,
        autorestart,
        max_restarts,
        restart_delay_ms: 0,
        kill_timeout_ms: 0,
        min_uptime_ms: 0,
    };

    let mut client = connect()?;
    let resp = client.service_start(config).map_err(client_err)?;
    ensure_ok("start", &resp)?;
    if let Some(svc) = resp.service_start.and_then(|r| r.service) {
        println!(
            "started '{}' (id={}, pid={}, status={})",
            svc.name, svc.id, svc.pid, svc.status
        );
    }
    Ok(())
}

fn cmd_list(json: bool) -> Result<()> {
    let mut client = connect()?;
    let resp = client.service_list().map_err(client_err)?;
    ensure_ok("list", &resp)?;
    let services = resp.service_list.map(|r| r.services).unwrap_or_default();
    if json {
        print_services_json(&services);
    } else {
        print_services_table(&services);
    }
    Ok(())
}

fn cmd_show(target: &str) -> Result<()> {
    let mut client = connect()?;
    let resp = client.service_describe(target).map_err(client_err)?;
    ensure_ok("show", &resp)?;
    match resp.service_describe.and_then(|r| r.service) {
        Some(svc) => print_service_detail(&svc),
        None => println!("no such service: {target}"),
    }
    Ok(())
}

fn cmd_logs(target: &str, lines: u32, follow: bool) -> Result<()> {
    let mut client = connect()?;
    let resp = client
        .service_logs(target, lines, follow)
        .map_err(client_err)?;
    if let Some(payload) = &resp.service_logs {
        if !payload.log_text.is_empty() {
            println!("{}", payload.log_text);
        }
    }
    print_status("logs", &resp)
}

fn cmd_simple_target<F>(label: &str, target: &str, call: F) -> Result<()>
where
    F: FnOnce(&mut DaemonClient, &str) -> Result<DaemonResponse, ClientError>,
{
    let mut client = connect()?;
    let resp = call(&mut client, target).map_err(client_err)?;
    ensure_ok(label, &resp)?;
    let count = match label {
        "stop" => resp.service_stop.as_ref().map(|r| r.stopped_count),
        "restart" => resp.service_restart.as_ref().map(|r| r.restarted_count),
        "delete" => resp.service_delete.as_ref().map(|r| r.deleted_count),
        _ => None,
    };
    match count {
        Some(n) => println!("{label}: {n} service(s)"),
        None => println!("OK: {label}"),
    }
    Ok(())
}

fn cmd_no_arg<F>(label: &str, call: F) -> Result<()>
where
    F: FnOnce(&mut DaemonClient) -> Result<DaemonResponse, ClientError>,
{
    let mut client = connect()?;
    let resp = call(&mut client).map_err(client_err)?;
    print_status(label, &resp)
}

fn cmd_phase4_stub(label: &str) -> Result<()> {
    println!("runpm: {label} not yet implemented (Phase 4 — see #106)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Maintenance subcommands (#228 Phase 1)
// ---------------------------------------------------------------------------

fn cmd_maintenance(command: MaintenanceCommands) -> Result<()> {
    match command {
        MaintenanceCommands::ReleaseHandles { path, json } => cmd_release_handles(path, json),
    }
}

fn cmd_release_handles(path: PathBuf, json: bool) -> Result<()> {
    let outcome = run_release_handles(&path)
        .with_context(|| format!("release-handles failed for {}", path.display()))?;
    if json {
        println!("{}", outcome.to_json());
    } else {
        println!("{}", outcome.message);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn connect() -> Result<DaemonClient> {
    DaemonClient::connect(None)
        .map_err(|_| anyhow!("daemon is not running (try `runpm --start-daemon`)"))
}

fn client_err(err: ClientError) -> anyhow::Error {
    anyhow!(err.to_string())
}

fn print_status(label: &str, resp: &DaemonResponse) -> Result<()> {
    ensure_ok(label, resp)?;
    println!("OK: {label}");
    Ok(())
}

/// Return an error if the daemon responded with a non-OK status code.
fn ensure_ok(label: &str, resp: &DaemonResponse) -> Result<()> {
    if resp.code == StatusCode::Ok as i32 {
        Ok(())
    } else {
        Err(anyhow!(
            "{}",
            if resp.message.is_empty() {
                format!("{label} failed (code={})", resp.code)
            } else {
                resp.message.clone()
            }
        ))
    }
}

// ---------------------------------------------------------------------------
// Service rendering
// ---------------------------------------------------------------------------

fn print_services_table(services: &[ServiceState]) {
    if services.is_empty() {
        println!("no services");
        return;
    }
    println!(
        "{:<4} {:<20} {:<10} {:<8} {:<8} COMMAND",
        "ID", "NAME", "STATUS", "PID", "RESTARTS"
    );
    for s in services {
        let command = s
            .config
            .as_ref()
            .map(|c| c.cmd.join(" "))
            .unwrap_or_default();
        println!(
            "{:<4} {:<20} {:<10} {:<8} {:<8} {}",
            s.id, s.name, s.status, s.pid, s.restart_count, command
        );
    }
}

fn print_service_detail(s: &ServiceState) {
    println!("name:          {}", s.name);
    println!("id:            {}", s.id);
    println!("status:        {}", s.status);
    println!("pid:           {}", s.pid);
    println!("restart_count: {}", s.restart_count);
    println!("last_exit:     {}", s.last_exit_code);
    if let Some(c) = &s.config {
        println!("command:       {}", c.cmd.join(" "));
        if !c.cwd.is_empty() {
            println!("cwd:           {}", c.cwd);
        }
        println!("autorestart:   {}", c.autorestart);
        println!("max_restarts:  {}", c.max_restarts);
    }
}

fn print_services_json(services: &[ServiceState]) {
    let values: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "status": s.status,
                "pid": s.pid,
                "restart_count": s.restart_count,
                "last_started_at": s.last_started_at,
                "last_exited_at": s.last_exited_at,
                "last_exit_code": s.last_exit_code,
                "command": s.config.as_ref().map(|c| c.cmd.clone()).unwrap_or_default(),
                "cwd": s.config.as_ref().map(|c| c.cwd.clone()).unwrap_or_default(),
                "autorestart": s.config.as_ref().map(|c| c.autorestart).unwrap_or(false),
                "max_restarts": s.config.as_ref().map(|c| c.max_restarts).unwrap_or(0),
            })
        })
        .collect();
    match serde_json::to_string_pretty(&values) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize JSON: {e}"),
    }
}

fn parse_env(args: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for entry in args {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| anyhow!("--env value `{entry}` must be in KEY=VALUE form"))?;
        if key.is_empty() {
            return Err(anyhow!("--env value `{entry}` has empty key"));
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn default_name_from(cmd: &str) -> Result<String> {
    let stem = Path::new(cmd)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty());
    stem.map(|s| s.to_string())
        .ok_or_else(|| anyhow!("could not derive default --name from `{cmd}`; pass --name"))
}
