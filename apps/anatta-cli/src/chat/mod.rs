//! `anatta chat` subcommand family.
//!
//! Subcommands:
//!   * `new <name> --profile <id> [--cwd <p>]` — start a fresh
//!     conversation
//!   * `resume <name>` — continue an existing conversation
//!   * `ls` — list conversations
//!   * `rm <name>` — delete a conversation row (refuses if in use)
//!
//! There is no `unlock` command: the per-conversation lock lives in
//! `anatta-runtime`'s [`SessionLock`](anatta_runtime::SessionLock),
//! which the OS releases automatically when the holding process
//! exits — stale-lock recovery is not a thing.

use std::path::PathBuf;

use anatta_runtime::SessionLock;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};

use crate::config::Config;

mod input;
mod render;
mod runner;

#[derive(Debug, Args)]
pub struct ChatArgs {
    #[command(subcommand)]
    pub action: ChatCommand,
}

#[derive(Debug, Subcommand)]
pub enum ChatCommand {
    /// Start a new named conversation against a profile.
    New {
        /// Conversation name (must be unique).
        name: String,
        /// Profile id (e.g., `claude-Ab12CdEf`).
        #[arg(long, short = 'p')]
        profile: String,
        /// Working directory the backend runs in (default: cwd).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Resume an existing conversation.
    Resume {
        /// Conversation name.
        name: String,
    },
    /// List conversations.
    Ls,
    /// Delete a conversation (refuses if in use).
    Rm {
        /// Conversation name.
        name: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("conversation '{0}' not found")]
    NotFound(String),
    #[error("conversation '{0}' already exists")]
    AlreadyExists(String),
    #[error("conversation '{0}' is in use by another anatta process")]
    Locked(String),
    #[error("profile not found: {0}")]
    ProfileNotFound(String),

    /// Sentinel for "user closed input" (Ctrl-D / Ctrl-C at prompt).
    /// Mapped to exit 0 with no message by `main.rs`.
    #[error("input closed")]
    InputClosed,

    #[error(transparent)]
    Send(#[from] crate::send::SendError),
    #[error(transparent)]
    Store(#[from] anatta_store::StoreError),
    #[error(transparent)]
    Spawn(#[from] anatta_runtime::spawn::SpawnError),
    #[error(transparent)]
    Lock(#[from] anatta_runtime::LockError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("readline: {0}")]
    Readline(String),
}

impl ChatError {
    /// Exit code this error maps to. `InputClosed` is success (0).
    pub fn exit_code(&self) -> i32 {
        match self {
            ChatError::InputClosed => 0,
            ChatError::NotFound(_)
            | ChatError::AlreadyExists(_)
            | ChatError::Locked(_)
            | ChatError::ProfileNotFound(_) => 2,
            _ => 1,
        }
    }

    /// Whether to print this error to stderr. `InputClosed` is silent.
    pub fn is_silent(&self) -> bool {
        matches!(self, ChatError::InputClosed)
    }
}

pub async fn run(args: ChatArgs, cfg: &Config) -> Result<(), ChatError> {
    match args.action {
        ChatCommand::New {
            name,
            profile,
            cwd,
        } => runner::run_new(name, profile, cwd, cfg).await,
        ChatCommand::Resume { name } => runner::run_resume(name, cfg).await,
        ChatCommand::Ls => run_ls(cfg).await,
        ChatCommand::Rm { name } => run_rm(name, cfg).await,
    }
}

async fn run_ls(cfg: &Config) -> Result<(), ChatError> {
    let rows = cfg.store.list_conversations().await?;
    if rows.is_empty() {
        eprintln!("no conversations. start one with `anatta chat new <name> --profile <id>`.");
        return Ok(());
    }
    let now = Utc::now();
    println!("{:<20} {:<22} {:<14} {}", "NAME", "PROFILE", "LAST USED", "STATUS");
    for row in rows {
        let status = if SessionLock::is_held(&cfg.anatta_home, &row.name) {
            "🔒 in use".to_owned()
        } else {
            "idle".to_owned()
        };
        let last = humanize_ago(now, row.last_used_at);
        println!(
            "{:<20} {:<22} {:<14} {}",
            truncate_for_col(&row.name, 20),
            truncate_for_col(&row.profile_id, 22),
            last,
            status,
        );
    }
    Ok(())
}

async fn run_rm(name: String, cfg: &Config) -> Result<(), ChatError> {
    // Existence check first so the error message is precise.
    if cfg.store.get_conversation(&name).await?.is_none() {
        return Err(ChatError::NotFound(name));
    }
    // Hold the SessionLock briefly: if we can't acquire it, someone
    // else is using the conversation right now. If we can, delete the
    // row and let the lock drop (the lockfile is left behind; it's
    // harmless and will be re-bound the next time someone with the
    // same name acquires).
    let _lock = SessionLock::try_acquire(&cfg.anatta_home, &name).map_err(|e| match e {
        anatta_runtime::LockError::Held { .. } => ChatError::Locked(name.clone()),
        anatta_runtime::LockError::Io(io) => ChatError::Io(io),
    })?;
    let deleted = cfg.store.delete_conversation(&name).await?;
    if !deleted {
        return Err(ChatError::NotFound(name));
    }
    eprintln!("removed conversation '{name}'");
    Ok(())
}

fn humanize_ago(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let dur = now.signed_duration_since(ts);
    let secs = dur.num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 2 {
        "yesterday".to_owned()
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else {
        ts.format("%Y-%m-%d").to_string()
    }
}

fn truncate_for_col(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_owned()
    } else if max <= 1 {
        "…".to_owned()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{head}…")
    }
}
