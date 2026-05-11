//! anatta CLI entry point.

mod auth;
mod chat;
mod config;
mod launch;
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
    /// Multi-turn chat against a profile (`new`, `resume`, `ls`, `rm`).
    Chat(chat::ChatArgs),
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

    let exit_code = match cli.command {
        Command::Profile { action } => match profile::run(action, &cfg).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("anatta: {e}");
                1
            }
        },
        Command::Send(args) => match send::run(args, &cfg).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("anatta: {e}");
                1
            }
        },
        Command::Chat(args) => match chat::run(args, &cfg).await {
            Ok(()) => 0,
            Err(e) => {
                if !e.is_silent() {
                    eprintln!("anatta: {e}");
                }
                e.exit_code()
            }
        },
    };
    std::process::exit(exit_code);
}
