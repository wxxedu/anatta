//! In-chat slash commands.
//!
//! Lines from the REPL that start with `/` are routed here instead of
//! being sent to the backend as prompts. v1 commands:
//!
//!   * `/profile`  — swap profile mid-chat (same-backend only)
//!   * `/exit`, `/quit` — leave the chat (same as Ctrl-D)
//!   * `/help`     — list available commands
//!
//! Cross-backend swap (`claude` → `codex` or vice versa) is rejected;
//! it needs a history-import pipeline that hasn't been designed yet.
//! The current `/profile` picker shows different-backend candidates
//! with a ⚠ marker so the user sees what's there but can't pick
//! through to corruption.

use anatta_store::profile::ProfileRecord;
use dialoguer::theme::ColorfulTheme;

use crate::config::Config;

use super::ChatError;

/// Outcome of a single slash-command invocation. Drives the chat
/// loop's state machine. `new_profile` is boxed because `ProfileRecord`
/// dwarfs the unit variants — without the box clippy flags the enum's
/// largest variant as 4-5× the size of the others.
pub(crate) enum SlashOutcome {
    /// No state change; loop back to next prompt.
    Continue,
    /// User wants to leave the chat (`/exit` / `/quit`).
    Exit,
    /// Picker chose a same-backend profile. Caller swaps in-place
    /// (claude: re-resolve api_key; codex: close + reopen session).
    SwapProfile { new_profile: Box<ProfileRecord> },
}

pub(crate) async fn handle(
    line: &str,
    current_profile: &ProfileRecord,
    cfg: &Config,
) -> Result<SlashOutcome, ChatError> {
    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "/profile" => handle_profile(current_profile, cfg).await,
        "/exit" | "/quit" => Ok(SlashOutcome::Exit),
        "/help" => {
            print_help();
            Ok(SlashOutcome::Continue)
        }
        other => {
            eprintln!("unknown command: {other} (try /help)");
            Ok(SlashOutcome::Continue)
        }
    }
}

fn print_help() {
    eprintln!(
        "available slash commands:\n  \
        /profile         swap to a different profile (same backend only)\n  \
        /exit, /quit     leave the chat (Ctrl-D works too)\n  \
        /help            show this list"
    );
}

async fn handle_profile(
    current: &ProfileRecord,
    cfg: &Config,
) -> Result<SlashOutcome, ChatError> {
    let profiles = cfg.store.list_profiles().await?;
    if profiles.is_empty() {
        eprintln!("no profiles configured");
        return Ok(SlashOutcome::Continue);
    }

    let labels: Vec<String> = profiles
        .iter()
        .map(|p| {
            let current_marker = if p.id == current.id { "★ " } else { "  " };
            let backend_warn = if p.backend != current.backend {
                "  ⚠ different backend (not supported)"
            } else {
                ""
            };
            format!(
                "{marker}{id}  ·  {backend}/{provider}  ·  {auth}  ·  \"{name}\"{warn}",
                marker = current_marker,
                id = p.id,
                backend = p.backend.as_str(),
                provider = p.provider,
                auth = p.auth_method.as_str(),
                name = p.name,
                warn = backend_warn,
            )
        })
        .collect();

    let default_idx = profiles
        .iter()
        .position(|p| p.id == current.id)
        .unwrap_or(0);

    let pick = dialoguer::Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Switch to which profile?")
        .items(&labels)
        .default(default_idx)
        .interact_opt()?;
    let Some(idx) = pick else {
        return Ok(SlashOutcome::Continue);
    };

    let new_profile = &profiles[idx];
    if new_profile.id == current.id {
        eprintln!("already on '{}'; no change", current.id);
        return Ok(SlashOutcome::Continue);
    }
    if new_profile.backend != current.backend {
        eprintln!(
            "✗ cross-backend swap not supported yet (current: {} · target: {}). \
             start a new chat against '{}' if you want to switch.",
            current.backend.as_str(),
            new_profile.backend.as_str(),
            new_profile.id,
        );
        return Ok(SlashOutcome::Continue);
    }

    Ok(SlashOutcome::SwapProfile {
        new_profile: Box::new(new_profile.clone()),
    })
}
