//! Chat loop entry point.
//!
//! Backend differences (claude per-turn spawn, codex persistent
//! app-server) are hidden inside [`Session`](anatta_runtime::spawn::Session);
//! the loop here is backend-agnostic.
//!
//! Per-turn cancellation goes through [`TurnEvents::cancel`]
//! (claude: SIGKILL the child with a 3s grace; codex: send
//! `turn/interrupt`, channel closes naturally and the session stays
//! alive for the next prompt).
//!
//! Profile swap (`/profile`) calls [`Session::swap`] with a freshly
//! resolved [`BackendLaunch`]; cross-backend swap is rejected at the
//! slash-command layer (see `super::slash`), defense-in-depth-rejected
//! again by `Session::swap` itself.

use std::path::PathBuf;
use std::sync::Arc;

use anatta_runtime::spawn::{Session, TurnEvents};
use anatta_runtime::{LockError, SessionLock};
use anatta_store::conversation::{ConversationRecord, NewConversation};
use anatta_store::profile::ProfileRecord;
use tokio::sync::Notify;

use super::input::{InputReader, ReadOutcome};
use super::render::line::LineRenderer;
use super::render::EventRenderer;
use super::slash::{self, SlashOutcome};
use super::ChatError;
use crate::config::Config;
use crate::launch;

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

    let result = run_chat(&conv, profile, cfg, &mut renderer, &mut input).await;

    renderer.on_chat_end();
    input.save_history();
    // _lock drops here — the OS releases the flock automatically.
    result
}

async fn run_chat(
    conv: &ConversationRecord,
    profile: ProfileRecord,
    cfg: &Config,
    renderer: &mut LineRenderer,
    input: &mut InputReader,
) -> Result<(), ChatError> {
    let cwd = PathBuf::from(&conv.cwd);
    let mut profile = profile;

    let launch = launch::build_launch(
        &profile,
        cwd.clone(),
        conv.backend_session_id.clone(),
        cfg,
    )?;
    let mut session = Session::open(launch).await?;

    // Persist the thread id on first turn (claude needs the system/init
    // event to capture it; codex gets it from thread/start). For codex
    // it's already known at open time.
    let mut backend_session_id = conv.backend_session_id.clone();
    if backend_session_id.is_none() {
        if let Some(id) = session.thread_id() {
            cfg.store.set_backend_session_id(&conv.name, id).await?;
            backend_session_id = Some(id.to_owned());
        }
    }

    let result: Result<(), ChatError> = loop {
        renderer.pre_prompt();
        match input.read_prompt() {
            ReadOutcome::Eof | ReadOutcome::Interrupted => {
                break Err(ChatError::InputClosed);
            }
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(s) if s.starts_with('/') => {
                match slash::handle(&s, &profile, cfg).await {
                    Ok(SlashOutcome::Continue) => continue,
                    Ok(SlashOutcome::Exit) => break Err(ChatError::InputClosed),
                    Ok(SlashOutcome::SwapProfile { new_profile }) => {
                        let new_profile = *new_profile;
                        let new_launch = match launch::build_launch(
                            &new_profile,
                            cwd.clone(),
                            backend_session_id.clone(),
                            cfg,
                        ) {
                            Ok(l) => l,
                            Err(e) => {
                                eprintln!("✗ build_launch failed: {e}");
                                continue;
                            }
                        };
                        if let Err(e) = session.swap(new_launch).await {
                            eprintln!("✗ swap failed: {e}");
                            continue;
                        }
                        if let Err(e) = cfg
                            .store
                            .set_conversation_profile(&conv.name, &new_profile.id)
                            .await
                        {
                            break Err(e.into());
                        }
                        eprintln!("→ swapped to profile '{}'", new_profile.id);
                        profile = new_profile;
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
            ReadOutcome::Line(prompt) => {
                let turn = match session.send_turn(&prompt).await {
                    Ok(t) => t,
                    Err(e) => break Err(e.into()),
                };
                if let Err(e) = drain_turn(turn, renderer).await {
                    break Err(e);
                }
                // For claude, first turn produced the session UUID;
                // persist if we didn't have one yet.
                if backend_session_id.is_none() {
                    if let Some(id) = session.thread_id() {
                        cfg.store.set_backend_session_id(&conv.name, id).await?;
                        backend_session_id = Some(id.to_owned());
                    }
                }
                cfg.store.touch_conversation(&conv.name).await?;
            }
        }
    };

    let _ = session.close().await;
    result
}

async fn drain_turn(
    mut turn: TurnEvents,
    renderer: &mut LineRenderer,
) -> Result<(), ChatError> {
    let cancel = Arc::new(Notify::new());
    let cancel_in_task = cancel.clone();
    let ctrl_c_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_in_task.notify_one();
    });

    let mut interrupted = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified(), if !interrupted => {
                interrupted = true;
                let _ = turn.cancel().await;
            }
            ev = turn.recv() => match ev {
                Some(e) => renderer.on_event(&e),
                None => break,
            }
        }
    }
    ctrl_c_task.abort();
    // For claude, harvest the per-turn child's exit info (otherwise
    // the child becomes a zombie until kill_on_drop fires). We don't
    // act on a non-zero exit here — the rendered Error events already
    // surfaced any failure to the user.
    let _ = turn.finalize().await;
    renderer.on_turn_end();
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
