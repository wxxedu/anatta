//! Chat loop entry points.
//!
//! Both `run_new` and `run_resume` end up in [`drive_chat`]: acquire the
//! lock, print a banner, loop on prompts, spawn one backend subprocess
//! per turn with `--resume <backend_session_id>`. Cancellation uses a
//! `Notify`-based pattern so `tokio::select!` doesn't need overlapping
//! `&mut session` borrows.

use std::path::PathBuf;
use std::sync::Arc;

use anatta_store::conversation::{ConversationRecord, NewConversation};
use anatta_store::profile::{BackendKind, ProfileRecord};
use anatta_runtime::spawn::{self, AgentSession};
use tokio::sync::Notify;

use super::input::{InputReader, ReadOutcome};
use super::lock::{ConversationGuard, LockError};
use super::render::line::LineRenderer;
use super::render::EventRenderer;
use super::ChatError;
use crate::config::Config;
use crate::send;

pub(crate) async fn run_new(
    name: String,
    profile_id: String,
    cwd: Option<PathBuf>,
    cfg: &Config,
) -> Result<(), ChatError> {
    let profile = cfg
        .store
        .get_profile(&profile_id)
        .await?
        .ok_or_else(|| ChatError::ProfileNotFound(profile_id.clone()))?;

    // Reject duplicate name before doing anything else.
    if cfg.store.get_conversation(&name).await?.is_some() {
        return Err(ChatError::AlreadyExists(name));
    }
    let cwd = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| ChatError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cwd is not valid UTF-8",
        )))?
        .to_owned();

    cfg.store
        .insert_conversation(NewConversation {
            name: &name,
            profile_id: &profile.id,
            cwd: &cwd_str,
        })
        .await?;

    let conv = cfg
        .store
        .get_conversation(&name)
        .await?
        .ok_or_else(|| ChatError::NotFound(name.clone()))?;
    drive_chat(conv, profile, cfg, /* resumed = */ false).await
}

pub(crate) async fn run_resume(name: String, cfg: &Config) -> Result<(), ChatError> {
    let conv = cfg
        .store
        .get_conversation(&name)
        .await?
        .ok_or_else(|| ChatError::NotFound(name.clone()))?;
    let profile = cfg
        .store
        .get_profile(&conv.profile_id)
        .await?
        .ok_or_else(|| ChatError::ProfileNotFound(conv.profile_id.clone()))?;
    drive_chat(conv, profile, cfg, /* resumed = */ true).await
}

async fn drive_chat(
    conv: ConversationRecord,
    profile: ProfileRecord,
    cfg: &Config,
    resumed: bool,
) -> Result<(), ChatError> {
    let guard = match ConversationGuard::try_acquire(&cfg.store, &conv.name).await {
        Ok(g) => g,
        Err(LockError::Held { pid }) => return Err(ChatError::Locked {
            name: conv.name.clone(),
            pid,
        }),
        Err(LockError::Store(s)) => return Err(ChatError::Store(s)),
    };

    print_banner(&conv, &profile, resumed);

    let mut renderer = LineRenderer::new();
    let mut input = InputReader::new(&cfg.anatta_home)?;
    let mut backend_session_id = conv.backend_session_id.clone();
    let cwd = PathBuf::from(&conv.cwd);

    let result: Result<(), ChatError> = loop {
        match input.read_prompt() {
            ReadOutcome::Eof => break Err(ChatError::InputClosed),
            ReadOutcome::Interrupted => break Err(ChatError::InputClosed),
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(prompt) => {
                match run_turn(
                    &profile,
                    &prompt,
                    backend_session_id.as_deref(),
                    cwd.clone(),
                    cfg,
                    &mut renderer,
                )
                .await
                {
                    Ok(session_id) => {
                        if backend_session_id.is_none() {
                            cfg.store
                                .set_backend_session_id(&conv.name, &session_id)
                                .await?;
                            backend_session_id = Some(session_id);
                        }
                        cfg.store.touch_conversation(&conv.name).await?;
                    }
                    Err(e) => {
                        // Turn-level failure: surface, stop the chat.
                        break Err(e);
                    }
                }
            }
        }
    };

    renderer.on_chat_end();
    input.save_history();
    // Release the lock; swallow release errors so the original chat
    // result wins.
    let _ = guard.release_now().await;
    result
}

/// Run a single turn end-to-end. Returns the backend session id (known
/// after the first event arrives).
async fn run_turn(
    profile: &ProfileRecord,
    prompt: &str,
    resume: Option<&str>,
    cwd: PathBuf,
    cfg: &Config,
    renderer: &mut LineRenderer,
) -> Result<String, ChatError> {
    let session = match profile.backend {
        BackendKind::Claude => {
            let launch = send::build_claude_launch(
                profile,
                prompt.to_owned(),
                resume.map(str::to_owned),
                cwd,
                cfg,
            )?;
            spawn::launch(launch).await?
        }
        BackendKind::Codex => {
            let launch = send::build_codex_launch(
                profile,
                prompt.to_owned(),
                resume.map(str::to_owned),
                cwd,
                cfg,
            )?;
            spawn::launch(launch).await?
        }
    };

    let session_id = session.session_id().to_owned();
    drain_with_cancellation(session, renderer).await?;
    Ok(session_id)
}

/// Drain events into the renderer until the backend exits, or Ctrl-C is
/// pressed. On Ctrl-C, cancel the child gracefully and return cleanly.
async fn drain_with_cancellation(
    mut session: AgentSession,
    renderer: &mut LineRenderer,
) -> Result<(), ChatError> {
    let cancel = Arc::new(Notify::new());
    let cancel_in_task = cancel.clone();
    let ctrl_c_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_in_task.notify_one();
    });

    let cancelled = loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break true,
            ev = session.events().recv() => match ev {
                Some(e) => renderer.on_event(&e),
                None => break false,
            }
        }
    };
    // Stop the ctrl-c listener so a later Ctrl-C reaches readline.
    ctrl_c_task.abort();

    let exit = if cancelled {
        session.cancel_mut().await?
    } else {
        session.wait().await?
    };
    renderer.on_turn_end(&exit);
    Ok(())
}

fn print_banner(conv: &ConversationRecord, profile: &ProfileRecord, resumed: bool) {
    let suffix = if resumed { " (resumed)" } else { "" };
    eprintln!();
    eprintln!("anatta chat · {name}{suffix}", name = conv.name);
    let session_hint = match (&conv.backend_session_id, resumed) {
        (Some(id), true) => {
            let short = if id.len() >= 8 { &id[..8] } else { id };
            format!("  ·  session: {short}…")
        }
        _ => String::new(),
    };
    eprintln!(
        "profile: {pid}  ·  cwd: {cwd}{session}",
        pid = profile.id,
        cwd = conv.cwd,
        session = session_hint,
    );
    let line = "─".repeat(70);
    eprintln!("{line}");
}
