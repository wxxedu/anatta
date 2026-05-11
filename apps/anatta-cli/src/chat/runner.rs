//! Chat loop entry points.
//!
//! `run_new` / `run_resume` set up the lock + banner + input reader,
//! then dispatch on the profile's backend kind:
//!
//! * **Claude**: spawn-per-turn. Each prompt creates a fresh
//!   `AgentSession` via `spawn::launch(ClaudeLaunch)`, drains events,
//!   the child exits, repeat.
//!
//! * **Codex**: persistent-per-chat. Open `codex app-server` once via
//!   `PersistentCodexSession::open`. Per prompt, `send_turn` issues a
//!   `turn/start` JSON-RPC request and returns a `TurnHandle` whose
//!   channel drains until `turn/completed`. The server stays alive
//!   between turns (the codex thread is kept hot — no handshake
//!   overhead, no per-turn 200ms penalty).
//!
//! Cancellation also differs:
//!
//! * Claude Ctrl-C: `session.cancel_mut()` → SIGTERM/SIGKILL the child.
//! * Codex Ctrl-C: `session.interrupt_current_turn()` → JSON-RPC
//!   `turn/interrupt`. codex emits `turn/completed { status:
//!   "interrupted" }` which closes the turn channel naturally; the
//!   session itself stays open and the next prompt continues.

use std::path::PathBuf;
use std::sync::Arc;

use anatta_runtime::spawn::{self, AgentSession, PersistentCodexSession, TurnHandle};
use anatta_runtime::{LockError, SessionLock};
use anatta_store::conversation::{ConversationRecord, NewConversation};
use anatta_store::profile::{BackendKind, ProfileRecord};
use tokio::sync::Notify;

use super::input::{InputReader, ReadOutcome};
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

    if cfg.store.get_conversation(&name).await?.is_some() {
        return Err(ChatError::AlreadyExists(name));
    }
    let cwd = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| {
            ChatError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cwd is not valid UTF-8",
            ))
        })?
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
    let _lock = match SessionLock::try_acquire(&cfg.anatta_home, &conv.name) {
        Ok(l) => l,
        Err(LockError::Held { .. }) => {
            return Err(ChatError::Locked(conv.name.clone()));
        }
        Err(LockError::Io(io)) => return Err(ChatError::Io(io)),
    };

    print_banner(&conv, &profile, resumed);

    let mut renderer = LineRenderer::new();
    let mut input = InputReader::new(&cfg.anatta_home)?;

    let result = match profile.backend {
        BackendKind::Claude => {
            run_chat_claude(&conv, &profile, cfg, &mut renderer, &mut input).await
        }
        BackendKind::Codex => {
            run_chat_codex(&conv, &profile, cfg, &mut renderer, &mut input).await
        }
    };

    renderer.on_chat_end();
    input.save_history();
    // _lock drops here — the OS releases the flock automatically.
    result
}

// ──────────────────────────────────────────────────────────────────────
// Claude: per-turn spawn
// ──────────────────────────────────────────────────────────────────────

async fn run_chat_claude(
    conv: &ConversationRecord,
    profile: &ProfileRecord,
    cfg: &Config,
    renderer: &mut LineRenderer,
    input: &mut InputReader,
) -> Result<(), ChatError> {
    let mut backend_session_id = conv.backend_session_id.clone();
    let cwd = PathBuf::from(&conv.cwd);

    loop {
        match input.read_prompt() {
            ReadOutcome::Eof | ReadOutcome::Interrupted => return Err(ChatError::InputClosed),
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(prompt) => {
                let launch = send::build_claude_launch(
                    profile,
                    prompt,
                    backend_session_id.clone(),
                    cwd.clone(),
                    cfg,
                )?;
                let session = spawn::launch(launch).await?;
                let session_id = session.session_id().to_owned();
                drain_claude_session(session, renderer).await?;
                if backend_session_id.is_none() {
                    cfg.store
                        .set_backend_session_id(&conv.name, &session_id)
                        .await?;
                    backend_session_id = Some(session_id);
                }
                cfg.store.touch_conversation(&conv.name).await?;
            }
        }
    }
}

async fn drain_claude_session(
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
    ctrl_c_task.abort();

    if cancelled {
        session.cancel_mut().await?;
    } else {
        session.wait().await?;
    }
    renderer.on_turn_end();
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Codex: persistent app-server, per-turn TurnHandle
// ──────────────────────────────────────────────────────────────────────

async fn run_chat_codex(
    conv: &ConversationRecord,
    profile: &ProfileRecord,
    cfg: &Config,
    renderer: &mut LineRenderer,
    input: &mut InputReader,
) -> Result<(), ChatError> {
    let cwd = PathBuf::from(&conv.cwd);

    // The CodexLaunch wants a `prompt` field, but for the persistent
    // open path we don't send a prompt yet (each turn supplies its
    // own). Pass an empty placeholder.
    let launch = send::build_codex_launch(
        profile,
        String::new(),
        conv.backend_session_id.clone(),
        cwd.clone(),
        cfg,
    )?;
    let session = PersistentCodexSession::open(launch).await?;

    // Persist the thread id immediately. For a fresh conversation
    // this is the first time we know it; for a resumed conversation
    // it should match the recorded value (no-op if already set).
    if conv.backend_session_id.is_none() {
        cfg.store
            .set_backend_session_id(&conv.name, session.thread_id())
            .await?;
    }

    let result: Result<(), ChatError> = loop {
        match input.read_prompt() {
            ReadOutcome::Eof | ReadOutcome::Interrupted => {
                break Err(ChatError::InputClosed);
            }
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(prompt) => {
                let turn = session.send_turn(&prompt).await?;
                if let Err(e) = drain_codex_turn(&session, turn, renderer).await {
                    break Err(e);
                }
                cfg.store.touch_conversation(&conv.name).await?;
            }
        }
    };

    // Graceful close: send EOF on stdin → codex exits cleanly.
    let _ = session.close().await;
    result
}

async fn drain_codex_turn(
    session: &PersistentCodexSession,
    mut turn: TurnHandle,
    renderer: &mut LineRenderer,
) -> Result<(), ChatError> {
    let cancel = Arc::new(Notify::new());
    let cancel_in_task = cancel.clone();
    let ctrl_c_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_in_task.notify_one();
    });

    // Codex cancellation = send turn/interrupt and keep draining.
    // codex emits turn/completed { status: "interrupted" } which
    // closes the channel naturally; the session stays open and the
    // next prompt continues.
    let mut interrupt_sent = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified(), if !interrupt_sent => {
                interrupt_sent = true;
                let _ = session.interrupt_current_turn().await;
                // Keep draining until turn/completed closes the channel.
            }
            ev = turn.events().recv() => match ev {
                Some(e) => renderer.on_event(&e),
                None => break,
            }
        }
    }
    ctrl_c_task.abort();
    renderer.on_turn_end();
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Shared
// ──────────────────────────────────────────────────────────────────────

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
