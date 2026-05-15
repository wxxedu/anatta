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

use super::ChatError;
use super::input::{InputReader, ReadOutcome};
use super::render::EventRenderer;
use super::render::line::LineRenderer;
use super::slash::{self, SlashOutcome};
use crate::config::Config;
use crate::conversation as orch;
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
    // Canonicalize so the encoded path matches what claude expects
    // (macOS /tmp → /private/tmp). conversation.cwd is immutable
    // afterwards, so we want the canonical form persisted.
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
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

    // Tier 1: ensure conversation has a ULID id + an active segment.
    // For legacy conversations (predating migration 0006), this
    // backfills the metadata and creates segment 0.
    let mut active_seg = orch::ensure_active_segment(cfg, &conv.name, &profile)
        .await
        .map_err(|e| ChatError::Io(std::io::Error::other(e.to_string())))?;
    let mut conv_meta = cfg
        .store
        .get_conversation_metadata(&conv.name)
        .await?
        .ok_or_else(|| ChatError::NotFound(conv.name.clone()))?;

    // Render the working file from central if we already have a
    // session_uuid (i.e., this is not the first turn). For first-turn
    // conversations, session_uuid is NULL → render is a no-op.
    if let Err(e) = orch::render_for_session(cfg, &conv_meta, &profile, &active_seg.id).await {
        return Err(ChatError::Io(std::io::Error::other(format!(
            "render failed: {e}"
        ))));
    }
    // Re-fetch segment row since render may have updated offsets.
    if let Some(s) = cfg
        .store
        .active_segment(conv_meta.id.as_deref().expect("conv_meta.id populated"))
        .await?
    {
        active_seg = s;
    }

    let launch = launch::build_launch(&profile, cwd.clone(), conv.backend_session_id.clone(), cfg)?;
    let mut session = Session::open(launch).await?;

    // Persist the engine session id on first turn (claude needs the
    // system/init event to capture it; codex gets it from thread/start).
    // For codex it's already known at open time. Tier 3: id lives on
    // the active segment row (with a back-compat write to
    // conversations.session_uuid too).
    let mut backend_session_id = active_seg.engine_session_id.clone();
    if backend_session_id.is_none() {
        if let Some(id) = session.thread_id() {
            orch::set_active_segment_engine_id_if_needed(cfg, &conv.name, &active_seg, id)
                .await
                .map_err(|e| ChatError::Io(std::io::Error::other(e.to_string())))?;
            backend_session_id = Some(id.to_owned());
            // Refresh local segment + metadata snapshots.
            if let Some(s) = cfg
                .store
                .active_segment(conv_meta.id.as_deref().expect("id"))
                .await?
            {
                active_seg = s;
            }
            if let Ok(Some(m)) = cfg.store.get_conversation_metadata(&conv.name).await {
                conv_meta = m;
            }
        }
    }

    let result: Result<(), ChatError> = loop {
        renderer.pre_prompt();
        match input.read_prompt() {
            ReadOutcome::Eof | ReadOutcome::Interrupted => {
                break Err(ChatError::InputClosed);
            }
            // TODO(task-12): implement permission-level cycling UI
            ReadOutcome::CyclePermission => continue,
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(s) if s.starts_with('/') => {
                match slash::handle(&s, &profile, cfg).await {
                    Ok(SlashOutcome::Continue) => continue,
                    Ok(SlashOutcome::Exit) => break Err(ChatError::InputClosed),
                    Ok(SlashOutcome::SwapProfile { new_profile }) => {
                        let new_profile = *new_profile;
                        let cross_engine = new_profile.backend != profile.backend;
                        // Final absorb of old segment before close (also harvests
                        // codex sub-agents if relevant).
                        if let Err(e) = orch::absorb_after_turn_for_session(
                            cfg,
                            &conv_meta,
                            &profile,
                            &active_seg,
                        )
                        .await
                        {
                            eprintln!("✗ pre-swap absorb failed: {e}");
                            continue;
                        }
                        // Open new segment row with family-aware transition policy
                        // + the new backend.
                        let new_active = match orch::open_segment_for_swap(
                            cfg,
                            &conv.name,
                            &new_profile,
                            /* ended_with_compact */ false,
                        )
                        .await
                        {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("✗ open new segment failed: {e}");
                                continue;
                            }
                        };
                        // Render under new profile. For cross-engine the foreign
                        // prior segments are routed through the transcoder cache.
                        // The new segment carries a pre-minted engine_session_id
                        // (set in open_segment_for_swap) so render can write the
                        // working file under a stable resume coordinate.
                        let render_outcome = match orch::render_for_session(
                            cfg,
                            &conv_meta,
                            &new_profile,
                            &new_active.id,
                        )
                        .await
                        {
                            Ok(o) => o,
                            Err(e) => {
                                eprintln!("✗ render under new profile failed: {e}");
                                let _ = cfg.store.close_segment(&new_active.id, false).await;
                                continue;
                            }
                        };
                        // If render produced no working content (all prior
                        // segments empty — e.g., user `/profile`d before
                        // any turn happened on the previous segment), don't
                        // resume against the empty file: claude / codex will
                        // bail. Start fresh and let the engine mint its own
                        // session id; we'll persist it post-turn,
                        // overwriting the placeholder.
                        let render_was_empty = matches!(
                            render_outcome,
                            anatta_runtime::conversation::RenderOutcome::SkippedFirstTurn
                        ) || matches!(
                            render_outcome,
                            anatta_runtime::conversation::RenderOutcome::Rendered {
                                working_bytes: 0
                            }
                        );
                        // Re-fetch to pick up the minted engine_session_id.
                        let new_active_after_render = cfg
                            .store
                            .get_segment(&new_active.id)
                            .await?
                            .expect("just opened");
                        let resume_id = if render_was_empty {
                            None
                        } else {
                            new_active_after_render.engine_session_id.clone()
                        };
                        let new_launch = match launch::build_launch(
                            &new_profile,
                            cwd.clone(),
                            resume_id.clone(),
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
                        if cross_engine {
                            eprintln!(
                                "→ swapped engine: {} → {} (profile '{}')",
                                profile.backend.as_str(),
                                new_profile.backend.as_str(),
                                new_profile.id
                            );
                        } else {
                            eprintln!("→ swapped to profile '{}'", new_profile.id);
                        }
                        profile = new_profile;
                        // The new segment now carries a pre-allocated
                        // engine_session_id (minted in open_segment_for_swap),
                        // and render has written the working file with
                        // transcoded prior content under that id. Reflect
                        // that on our local tracker.
                        backend_session_id = resume_id;
                        // Refresh active_seg + conv_meta after render mutations.
                        if let Some(s) = cfg
                            .store
                            .active_segment(conv_meta.id.as_deref().expect("id"))
                            .await?
                        {
                            active_seg = s;
                        }
                        if let Ok(Some(m)) = cfg.store.get_conversation_metadata(&conv.name).await {
                            conv_meta = m;
                        }
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
                // persist on the active segment if we didn't have one yet.
                if backend_session_id.is_none() {
                    if let Some(id) = session.thread_id() {
                        orch::set_active_segment_engine_id_if_needed(
                            cfg,
                            &conv.name,
                            &active_seg,
                            id,
                        )
                        .await
                        .map_err(|e| ChatError::Io(std::io::Error::other(e.to_string())))?;
                        backend_session_id = Some(id.to_owned());
                        // Refresh metadata + segment now that ids are populated.
                        if let Some(s) = cfg
                            .store
                            .active_segment(conv_meta.id.as_deref().expect("id"))
                            .await?
                        {
                            active_seg = s;
                        }
                        if let Ok(Some(m)) = cfg.store.get_conversation_metadata(&conv.name).await {
                            conv_meta = m;
                        }
                    }
                }
                cfg.store.touch_conversation(&conv.name).await?;
                // tier 1: absorb new bytes after each turn into central
                if let Err(e) =
                    orch::absorb_after_turn_for_session(cfg, &conv_meta, &profile, &active_seg)
                        .await
                {
                    eprintln!("✗ post-turn absorb failed: {e}");
                }
                // Refresh active_seg so its last_absorbed_bytes is current.
                if let Some(s) = cfg
                    .store
                    .active_segment(conv_meta.id.as_deref().expect("id"))
                    .await?
                {
                    active_seg = s;
                }
            }
        }
    };

    let _ = session.close().await;
    // tier 1: final absorb + cleanup at session end
    if let Err(e) = orch::finalize_session(cfg, &conv_meta, &profile, &active_seg).await {
        eprintln!("✗ session finalize failed: {e}");
    }
    result
}

async fn drain_turn(mut turn: TurnEvents, renderer: &mut LineRenderer) -> Result<(), ChatError> {
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
