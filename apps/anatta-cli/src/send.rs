//! `anatta send` — one-shot prompt through a configured profile.
//!
//! Uses the runtime's [`Session`] abstraction: open → send_turn → drain
//! → close. Backend differences (claude per-turn spawn vs codex
//! persistent app-server) are hidden inside `Session`.

use std::path::PathBuf;

use anatta_core::{AgentEvent, AgentEventPayload};
use anatta_runtime::spawn::{BackendKind, ExitInfo, Session};
use anatta_runtime::{LockError, SessionLock};
use clap::Args;

use crate::config::Config;
use crate::conversation as orch;
use crate::launch::{self, LaunchError};

#[derive(Args, Debug)]
pub struct SendArgs {
    /// Profile id (e.g. `claude-Ab12CdEf` or `codex-…`).
    profile: String,
    /// Prompt text.
    prompt: String,
    /// Working directory the agent runs in. Defaults to the current dir.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Resume an existing session/thread (claude session UUID / codex
    /// thread UUID) instead of starting fresh.
    #[arg(long)]
    resume: Option<String>,
    /// Emit each `AgentEvent` as one JSON line on stdout instead of a
    /// pretty-printed transcript.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error(
        "backend `{backend}` exited with code={code:?} signal={signal:?}; \
         stderr tail: {stderr_tail}"
    )]
    BackendNonZero {
        backend: &'static str,
        code: Option<i32>,
        signal: Option<i32>,
        stderr_tail: String,
    },
    #[error(
        "session id matches conversation '{0}' which is in use by another anatta process"
    )]
    ConversationLocked(String),

    #[error("launch: {0}")]
    Launch(#[from] LaunchError),
    #[error("store: {0}")]
    Store(#[from] anatta_store::StoreError),
    #[error("spawn: {0}")]
    Spawn(#[from] anatta_runtime::spawn::SpawnError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize event: {0}")]
    Json(#[from] serde_json::Error),
}

pub async fn run(args: SendArgs, cfg: &Config) -> Result<(), SendError> {
    let record = cfg
        .store
        .get_profile(&args.profile)
        .await?
        .ok_or_else(|| SendError::ProfileNotFound(args.profile.clone()))?;

    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    let _lock = maybe_acquire_chat_lock(args.resume.as_deref(), cfg).await?;
    // Tier 1: if --resume points to a known conversation, bootstrap segments
    // and render the working file under the (matching profile's) projects/.
    let segment_ctx = if let Some((conv_name, _)) = &_lock {
        match orch::ensure_active_segment(cfg, conv_name, &record).await {
            Ok(seg) => {
                let meta = cfg
                    .store
                    .get_conversation_metadata(conv_name)
                    .await?
                    .map(|m| (conv_name.clone(), seg, m));
                if let Some((_n, seg, meta)) = &meta {
                    if let Err(e) =
                        orch::render_for_session(cfg, meta, &record, &seg.id).await
                    {
                        eprintln!("[anatta] warn: render failed before send: {e}");
                    }
                }
                meta
            }
            Err(e) => {
                eprintln!("[anatta] warn: segment bootstrap failed: {e}");
                None
            }
        }
    } else {
        None
    };

    // Refresh segment after render adjusted offsets.
    let segment_ctx = if let Some((n, _, meta)) = segment_ctx {
        if let Ok(Some(seg)) = cfg
            .store
            .active_segment(meta.id.as_deref().unwrap_or(""))
            .await
        {
            Some((n, seg, meta))
        } else {
            None
        }
    } else {
        None
    };

    let launch = launch::build_launch(&record, cwd, args.resume, cfg)?;
    let mut session = Session::open(launch).await?;
    let kind = session.kind();
    if let Some(id) = session.thread_id() {
        eprintln!("[anatta] session: {id}");
    }
    // First-turn case for send: if the active segment's engine_session_id
    // is NULL and we just learned it from the spawned CLI, persist on the
    // segment (with back-compat dual-write to conversations.session_uuid).
    if let Some((conv_name, seg, _)) = &segment_ctx {
        if let Some(id) = session.thread_id() {
            orch::set_active_segment_engine_id_if_needed(cfg, conv_name, seg, id)
                .await
                .map_err(|e| SendError::Io(std::io::Error::other(e.to_string())))?;
        }
    }

    let mut turn = session.send_turn(&args.prompt).await?;
    while let Some(ev) = turn.recv().await {
        if args.json {
            println!("{}", serde_json::to_string(&ev)?);
        } else {
            render_pretty(&ev);
        }
    }
    // Claude harvests its per-turn ExitInfo here; codex returns None
    // (its session-level exit comes from Session::close below).
    let per_turn_exit = turn.finalize().await?;
    let session_exit = session.close().await?;

    // Tier 1: absorb new bytes into central + finalize (deletes working).
    if let Some((conv_name, seg, _)) = &segment_ctx {
        if let Ok(Some(meta)) = cfg.store.get_conversation_metadata(conv_name).await {
            if let Err(e) =
                orch::absorb_after_turn_for_session(cfg, &meta, &record, seg).await
            {
                eprintln!("[anatta] warn: absorb after send failed: {e}");
            }
            // finalize: send is one-shot, so this is the end of the session.
            if let Err(e) = orch::finalize_session(cfg, &meta, &record, seg).await {
                eprintln!("[anatta] warn: finalize after send failed: {e}");
            }
        }
    }

    cfg.store.touch_profile(&record.id).await?;
    if let Some((name, _lock)) = &_lock {
        cfg.store.touch_conversation(name).await?;
        let _ = _lock; // explicit scope marker; OS releases on drop
    }

    let exit = per_turn_exit.or(session_exit).ok_or_else(|| {
        SendError::Io(std::io::Error::other(
            "backend produced neither per-turn nor session-level exit info",
        ))
    })?;
    let backend_name = match kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
    };
    eprintln!(
        "[anatta] exit: code={:?} signal={:?} duration={:?} events={}",
        exit.exit_code, exit.signal, exit.duration, exit.events_emitted,
    );
    enforce_exit(backend_name, exit)
}

/// If `--resume <id>` targets a backend_session_id recorded against a
/// named conversation, acquire its [`SessionLock`] so a concurrent
/// `anatta chat resume` is blocked. Returns `None` if no matching
/// conversation exists (ad-hoc resume is allowed without coordination).
async fn maybe_acquire_chat_lock(
    resume: Option<&str>,
    cfg: &Config,
) -> Result<Option<(String, SessionLock)>, SendError> {
    let Some(id) = resume else {
        return Ok(None);
    };
    let Some(conv) = cfg.store.get_conversation_by_backend_session_id(id).await? else {
        return Ok(None);
    };
    match SessionLock::try_acquire(&cfg.anatta_home, &conv.name) {
        Ok(lock) => Ok(Some((conv.name, lock))),
        Err(LockError::Held { .. }) => Err(SendError::ConversationLocked(conv.name)),
        Err(LockError::Io(io)) => Err(SendError::Io(io)),
    }
}

fn render_pretty(ev: &AgentEvent) {
    use AgentEventPayload::*;
    match &ev.payload {
        SessionStarted { model, cwd, .. } => {
            eprintln!("[anatta] model={model} cwd={cwd}");
        }
        AssistantText { text } => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        Thinking { text } => {
            for line in text.lines() {
                eprintln!("  · {line}");
            }
        }
        ToolUse { name, input, .. } => {
            let summary = serde_json::to_string(input).unwrap_or_default();
            eprintln!("[tool] {name} {}", truncate(&summary, 140));
        }
        ToolResult { success, text, .. } => {
            let tag = if *success { "ok" } else { "err" };
            let body = text.as_deref().unwrap_or("");
            eprintln!("[tool/{tag}] {}", truncate(body, 200));
        }
        Usage {
            input_tokens,
            output_tokens,
            cost_usd,
            ..
        } => match cost_usd {
            Some(c) => eprintln!("[usage] in={input_tokens} out={output_tokens} cost=${c:.4}"),
            None => eprintln!("[usage] in={input_tokens} out={output_tokens}"),
        },
        TurnCompleted {
            stop_reason,
            is_error,
        } => {
            let err = if *is_error { " (error)" } else { "" };
            match stop_reason {
                Some(r) => eprintln!("[turn] done ({r}){err}"),
                None => eprintln!("[turn] done{err}"),
            }
        }
        RateLimit {
            limit_kind,
            used_percent,
            status,
            ..
        } => {
            let pct = used_percent
                .map(|p| format!(" {p:.0}%"))
                .unwrap_or_default();
            let st = status.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default();
            eprintln!("[rate-limit] {limit_kind}{pct}{st}");
        }
        Error { message, fatal } => {
            eprintln!("[error{}] {message}", if *fatal { " fatal" } else { "" });
        }
        TurnStarted | UserPrompt { .. } => {}
        AssistantTextDelta { .. } | ThinkingDelta { .. } | ToolUseInputDelta { .. } => {}
    }
}

fn truncate(s: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max_chars {
        std::borrow::Cow::Borrowed(s)
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        std::borrow::Cow::Owned(format!("{truncated}…"))
    }
}

fn enforce_exit(backend: &'static str, exit: ExitInfo) -> Result<(), SendError> {
    if exit.exit_code == Some(0) {
        Ok(())
    } else {
        Err(SendError::BackendNonZero {
            backend,
            code: exit.exit_code,
            signal: exit.signal,
            stderr_tail: exit.stderr_tail,
        })
    }
}
