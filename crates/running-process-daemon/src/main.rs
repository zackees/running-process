use clap::{Parser, Subcommand};

use running_process_daemon::{client, paths, server};
use running_process_proto::daemon::{StatusCode, TrackedProcess};

#[derive(Parser)]
#[command(
    name = "running-process-daemon",
    about = "Daemon for subprocess tracking"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon in the background.
    ///
    /// Without any flags the daemon listens on the default global socket.
    /// Tests and parallel daemons can use --scope to derive an isolated
    /// socket/db path, or override either path directly.
    Start {
        /// Use this scope name instead of the global default. The socket
        /// and SQLite paths are derived from the scope.
        #[arg(long)]
        scope: Option<String>,
        /// Override the IPC socket path (skips scope-based derivation).
        #[arg(long)]
        socket_path: Option<String>,
        /// Override the SQLite database path (skips scope-based derivation).
        #[arg(long)]
        db_path: Option<String>,
    },
    /// Stop the running daemon
    Stop,
    /// Check if the daemon is alive
    Ping,
    /// Show daemon status
    Status,
    /// List tracked processes
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        originator: Option<String>,
    },
    /// Find and kill zombie processes
    KillZombies {
        #[arg(long)]
        dry_run: bool,
    },
    /// Kill a specific process tree
    Kill { pid: u32 },
    /// Show process tree
    Tree { pid: u32 },
    /// Detachable PTY and pipe sessions (issue #130)
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List PTY and pipe sessions owned by the daemon.
    List {
        /// Filter by originator string. Empty matches all.
        #[arg(long, default_value = "")]
        originator: String,
        /// Show only PTY sessions.
        #[arg(long, conflicts_with = "pipe")]
        pty: bool,
        /// Show only pipe sessions.
        #[arg(long, conflicts_with = "pty")]
        pipe: bool,
    },
    /// Schedule termination of a session.
    Terminate {
        session_id: String,
        /// Soft signal grace window before hard kill (milliseconds).
        #[arg(long, default_value = "2000")]
        grace_ms: u32,
        /// Force pipe-session interpretation. Default: try PTY first,
        /// fall back to pipe.
        #[arg(long)]
        pipe: bool,
    },
}

/// Initialize structured logging via `tracing-subscriber`.
///
/// Logs go to stderr (standard daemon practice).  The level is controlled by
/// the `RUST_LOG` environment variable and defaults to `info`.
fn init_logging() {
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start {
            scope,
            socket_path,
            db_path,
        } => {
            init_logging();
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(async {
                let scope_name = scope.clone().unwrap_or_else(|| "global".to_string());
                let socket =
                    socket_path.unwrap_or_else(|| paths::socket_path(scope.as_deref()));
                let db = db_path.unwrap_or_else(|| {
                    paths::db_path(scope.as_deref())
                        .to_string_lossy()
                        .into_owned()
                });
                let cwd = std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let srv = match server::DaemonServer::new(
                    socket,
                    db,
                    scope_name,
                    scope.unwrap_or_default(),
                    cwd,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("failed to initialize daemon: {e}");
                        std::process::exit(1);
                    }
                };
                if let Err(e) = srv.run().await {
                    eprintln!("daemon error: {e}");
                    std::process::exit(1);
                }
            });
        }
        Commands::Stop => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.shutdown(true, 5.0) {
                Ok(_resp) => println!("daemon is shutting down"),
                Err(e) => eprintln!("shutdown failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Ping => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.ping() {
                Ok(resp) => println!(
                    "pong (server time: {}ms)",
                    resp.ping.map(|p| p.server_time_ms).unwrap_or(0)
                ),
                Err(e) => eprintln!("ping failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Status => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.status() {
                Ok(resp) => {
                    if let Some(s) = resp.status {
                        println!("version:          {}", s.version);
                        println!("uptime:           {}s", s.uptime_seconds);
                        println!("tracked procs:    {}", s.tracked_process_count);
                        println!("active conns:     {}", s.active_connections);
                        println!("socket:           {}", s.socket_path);
                        println!("db:               {}", s.db_path);
                        if !s.scope.is_empty() {
                            println!("scope:            {}", s.scope);
                            println!("scope_hash:       {}", s.scope_hash);
                            println!("scope_cwd:        {}", s.scope_cwd);
                        }
                    } else {
                        println!("status: ok (no details)");
                    }
                }
                Err(e) => eprintln!("status failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::List { json, originator } => match client::DaemonClient::connect(None) {
            Ok(mut c) => {
                let resp = if let Some(tool) = &originator {
                    c.list_by_originator(tool)
                } else {
                    c.list_active()
                };
                match resp {
                    Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                        let processes = resp
                            .list_active
                            .map(|r| r.processes)
                            .or_else(|| resp.list_by_originator.map(|r| r.processes))
                            .unwrap_or_default();

                        if json {
                            print_json(&processes);
                        } else {
                            print_table(&processes);
                        }
                    }
                    Ok(resp) => eprintln!("error: {}", resp.message),
                    Err(e) => eprintln!("list failed: {e}"),
                }
            }
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::KillZombies { dry_run } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.kill_zombies(dry_run) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let zombies = resp.kill_zombies.map(|r| r.zombies).unwrap_or_default();
                    if zombies.is_empty() {
                        println!("no zombies found");
                    } else {
                        for z in &zombies {
                            let action = if z.killed {
                                "killed"
                            } else {
                                "found (dry-run)"
                            };
                            println!(
                                "  PID {} — {} — {} [{}]",
                                z.pid, z.command, z.reason, action
                            );
                        }
                        println!(
                            "{} zombie(s) {}",
                            zombies.len(),
                            if dry_run { "found" } else { "killed" }
                        );
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("kill-zombies failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Kill { pid } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.kill_tree(pid, 3.0) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let count = resp.kill_tree.map(|r| r.processes_killed).unwrap_or(0);
                    println!("killed {} process(es) in tree for PID {}", count, pid);
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("kill failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Tree { pid } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.get_process_tree(pid) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let display = resp
                        .get_process_tree
                        .map(|r| r.tree_display)
                        .unwrap_or_default();
                    if display.is_empty() {
                        println!("no process tree found for PID {}", pid);
                    } else {
                        println!("{}", display);
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("tree failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Sessions { command } => run_sessions_command(command),
    }
}

fn run_sessions_command(command: SessionsCommand) {
    let mut client = match client::DaemonClient::connect(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("daemon is not running");
            std::process::exit(1);
        }
    };
    match command {
        SessionsCommand::List {
            originator,
            pty,
            pipe,
        } => {
            let show_pty = pty || (!pty && !pipe);
            let show_pipe = pipe || (!pty && !pipe);
            if show_pty {
                match client.list_pty_sessions(&originator) {
                    Ok(sessions) => print_pty_session_table(&sessions),
                    Err(e) => eprintln!("list_pty_sessions failed: {e}"),
                }
            }
            if show_pipe {
                match client.list_pipe_sessions(&originator) {
                    Ok(sessions) => print_pipe_session_table(&sessions),
                    Err(e) => eprintln!("list_pipe_sessions failed: {e}"),
                }
            }
        }
        SessionsCommand::Terminate {
            session_id,
            grace_ms,
            pipe,
        } => {
            if pipe {
                match client.terminate_pipe_session(&session_id, grace_ms) {
                    Ok(()) => println!("pipe session {session_id} terminate scheduled"),
                    Err(e) => {
                        eprintln!("terminate_pipe_session failed: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                // Try PTY first, fall back to pipe on NotFound.
                match client.terminate_pty_session(&session_id, grace_ms) {
                    Ok(()) => println!("pty session {session_id} terminate scheduled"),
                    Err(client::ClientError::Server { code, .. })
                        if code == StatusCode::NotFound =>
                    {
                        match client.terminate_pipe_session(&session_id, grace_ms) {
                            Ok(()) => {
                                println!("pipe session {session_id} terminate scheduled")
                            }
                            Err(e) => {
                                eprintln!("terminate failed: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("terminate_pty_session failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn print_pty_session_table(sessions: &[running_process_proto::daemon::PtySessionInfo]) {
    if sessions.is_empty() {
        println!("no PTY sessions");
        return;
    }
    println!("PTY sessions:");
    println!(
        "  {:<48} {:<7} {:<10} {:<10} {}",
        "SESSION_ID", "PID", "STATE", "ATTACHED", "COMMAND"
    );
    for s in sessions {
        let state = if s.exited {
            format!("exit({})", s.exit_code)
        } else {
            "running".into()
        };
        let attached = if s.attached { "yes" } else { "no" };
        println!(
            "  {:<48} {:<7} {:<10} {:<10} {}",
            s.session_id, s.pid, state, attached, s.command
        );
    }
}

fn print_pipe_session_table(sessions: &[running_process_proto::daemon::PipeSessionInfo]) {
    if sessions.is_empty() {
        println!("no pipe sessions");
        return;
    }
    println!("Pipe sessions:");
    println!(
        "  {:<48} {:<7} {:<10} {:<12} {}",
        "SESSION_ID", "PID", "STATE", "OUT/ERR ATT", "COMMAND"
    );
    for s in sessions {
        let state = if s.exited {
            format!("exit({})", s.exit_code)
        } else {
            "running".into()
        };
        let attached = format!(
            "{}/{}",
            if s.stdout_attached { "y" } else { "n" },
            if s.stderr_attached { "y" } else { "n" }
        );
        println!(
            "  {:<48} {:<7} {:<10} {:<12} {}",
            s.session_id, s.pid, state, attached, s.command
        );
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Format an uptime duration in seconds as a human-readable string.
fn format_uptime(seconds: f64) -> String {
    let secs = seconds as u64;
    if secs < 60 {
        return format!("{}s", secs);
    }
    if secs < 3600 {
        return format!("{}m {}s", secs / 60, secs % 60);
    }
    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
}

/// Map a protobuf `ProcessState` i32 value to a human-readable name.
fn state_name(state: i32) -> &'static str {
    match state {
        1 => "alive",
        2 => "dead",
        3 => "zombie",
        _ => "unknown",
    }
}

/// Print a list of tracked processes as a formatted table.
fn print_table(processes: &[TrackedProcess]) {
    if processes.is_empty() {
        println!("no tracked processes");
        return;
    }

    println!(
        "{:<8} {:<8} {:<12} {:<8} COMMAND",
        "PID", "STATE", "KIND", "UPTIME"
    );
    for p in processes {
        println!(
            "{:<8} {:<8} {:<12} {:<8} {}",
            p.pid,
            state_name(p.state),
            p.kind,
            format_uptime(p.uptime_seconds),
            p.command,
        );
    }
}

/// Print a list of tracked processes as JSON.
fn print_json(processes: &[TrackedProcess]) {
    let json_values: Vec<serde_json::Value> = processes
        .iter()
        .map(|p| {
            serde_json::json!({
                "pid": p.pid,
                "state": state_name(p.state),
                "kind": p.kind,
                "command": p.command,
                "cwd": p.cwd,
                "originator": p.originator,
                "containment": p.containment,
                "created_at": p.created_at,
                "registered_at": p.registered_at,
                "uptime_seconds": p.uptime_seconds,
                "parent_alive": p.parent_alive,
                "last_validated_at": p.last_validated_at,
            })
        })
        .collect();

    match serde_json::to_string_pretty(&json_values) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize JSON: {e}"),
    }
}
