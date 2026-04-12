use clap::{Parser, Subcommand};

use running_process_daemon::{client, paths, server};

#[derive(Parser)]
#[command(name = "running-process-daemon", about = "Daemon for subprocess tracking")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon in the background
    Start,
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
    Kill {
        pid: u32,
    },
    /// Show process tree
    Tree {
        pid: u32,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start => {
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(async {
                let socket = paths::socket_path(None);
                let db = paths::db_path(None).to_string_lossy().into_owned();
                let srv = server::DaemonServer::new(
                    socket,
                    db,
                    "global".to_string(),
                    String::new(),
                    String::new(),
                );
                if let Err(e) = srv.run().await {
                    eprintln!("daemon error: {e}");
                    std::process::exit(1);
                }
            });
        }
        Commands::Stop => {
            match client::DaemonClient::connect(None) {
                Ok(mut c) => match c.shutdown(true, 5.0) {
                    Ok(_resp) => println!("daemon is shutting down"),
                    Err(e) => eprintln!("shutdown failed: {e}"),
                },
                Err(_) => eprintln!("daemon is not running"),
            }
        }
        Commands::Ping => {
            match client::DaemonClient::connect(None) {
                Ok(mut c) => match c.ping() {
                    Ok(resp) => println!(
                        "pong (server time: {}ms)",
                        resp.ping.map(|p| p.server_time_ms).unwrap_or(0)
                    ),
                    Err(e) => eprintln!("ping failed: {e}"),
                },
                Err(_) => eprintln!("daemon is not running"),
            }
        }
        Commands::Status => {
            match client::DaemonClient::connect(None) {
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
            }
        }
        Commands::List { json: _, originator: _ } => println!("listing processes..."),
        Commands::KillZombies { dry_run: _ } => println!("killing zombies..."),
        Commands::Kill { pid } => println!("killing process tree {}...", pid),
        Commands::Tree { pid } => println!("showing tree for {}...", pid),
    }
}
