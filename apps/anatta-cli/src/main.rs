//! anatta CLI entry point.

mod auth;
mod config;
mod profile;
mod send;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "anatta",
    version,
    about = "anatta — orchestrate remote claude/codex sessions"
)]
struct Cli {
    /// Override the anatta home directory (default: $ANATTA_HOME or ~/.anatta).
    #[arg(long, global = true)]
    anatta_home: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Manage backend profiles (claude / codex accounts).
    Profile {
        #[command(subcommand)]
        action: profile::ProfileCommand,
    },
    /// Send a one-shot prompt through a profile and stream the response.
    Send(send::SendArgs),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let cfg = match config::Config::resolve(cli.anatta_home).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("anatta: {e}");
            std::process::exit(2);
        }
    };

    let result = match cli.command {
        Command::Profile { action } => profile::run(action, &cfg).await.map_err(|e| e.to_string()),
        Command::Send(args) => send::run(args, &cfg).await.map_err(|e| e.to_string()),
    };

    if let Err(msg) = result {
        eprintln!("anatta: {msg}");
        std::process::exit(1);
    }
}
