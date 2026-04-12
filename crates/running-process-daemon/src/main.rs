use clap::{Parser, Subcommand};

mod platform;

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
        Commands::Start => println!("starting daemon..."),
        Commands::Stop => println!("stopping daemon..."),
        Commands::Ping => println!("pinging daemon..."),
        Commands::Status => println!("daemon status..."),
        Commands::List { json: _, originator: _ } => println!("listing processes..."),
        Commands::KillZombies { dry_run: _ } => println!("killing zombies..."),
        Commands::Kill { pid } => println!("killing process tree {}...", pid),
        Commands::Tree { pid } => println!("showing tree for {}...", pid),
    }
}
