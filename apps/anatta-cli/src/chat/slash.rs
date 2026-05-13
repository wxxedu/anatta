//! In-chat slash commands.
//!
//! Lines from the REPL that start with `/` are routed here instead of
//! being sent to the backend as prompts. v1 commands:
//!
//!   * `/profile`  — swap profile mid-chat (any backend, tier 3)
//!   * `/exit`, `/quit` — leave the chat (same as Ctrl-D)
//!   * `/help`     — list available commands
//!
//! Tier 3 allows cross-engine swap. The picker lists profiles from
//! both backends; cross-engine choice triggers an explicit confirm
//! noting reasoning blocks will be dropped from prior foreign-engine
//! segments (text + tool calls / results are preserved by the
//! transcoder).

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
    /// Picker chose a profile. Caller swaps in-place. Cross-engine
    /// swaps are handled by the same outcome — the runtime layer
    /// re-opens the session and the orchestration layer re-renders
    /// transcoded history.
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
        /profile         swap to a different profile (any backend)\n  \
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

    let current_family_override = current.family_override.as_deref();

    let labels: Vec<String> = profiles
        .iter()
        .map(|p| {
            let current_marker = if p.id == current.id { "★ " } else { "  " };
            let kind_marker = if p.id == current.id {
                "" // active row — no extra marker
            } else if p.backend != current.backend {
                "  ⇄ different engine"
            } else if p.family_override.as_deref() != current_family_override
                || p.provider != current.provider
            {
                "  ⓘ different family/provider"
            } else {
                ""
            };
            format!(
                "{marker}{id}  ·  {backend}/{provider}  ·  {auth}  ·  \"{name}\"{kind}",
                marker = current_marker,
                id = p.id,
                backend = p.backend.as_str(),
                provider = p.provider,
                auth = p.auth_method.as_str(),
                name = p.name,
                kind = kind_marker,
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

    // Cross-engine swap requires explicit confirmation noting the
    // information loss boundary (reasoning blocks dropped from prior
    // foreign-engine segments; text + tool calls / results preserved).
    if new_profile.backend != current.backend {
        eprintln!(
            "Switching engine: {} → {}.\n  \
             ▸ Text and tool calls/results from prior segments are preserved.\n  \
             ▸ Reasoning blocks (thinking/reasoning) from foreign-engine segments are dropped\n    \
               (per-engine signatures/encrypted_content cannot cross).\n  \
             ▸ Your own engine's reasoning is preserved when you eventually switch back.\n",
            current.backend.as_str(),
            new_profile.backend.as_str(),
        );
        let confirm = dialoguer::Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Switch to '{}'?", new_profile.id))
            .default(false)
            .interact_opt()?;
        match confirm {
            Some(true) => {}
            _ => return Ok(SlashOutcome::Continue),
        }
    }

    Ok(SlashOutcome::SwapProfile {
        new_profile: Box::new(new_profile.clone()),
    })
}
