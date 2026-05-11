//! `anatta send` — one-shot prompt through a configured profile.
//!
//! Resolves the profile's provider + per-row overrides into a flat
//! `(name, value)` env list via [`anatta_runtime::profile::ProviderEnv`]
//! (api-key path) or skips env injection entirely (login path; claude-cli
//! reads its own keychain), spawns the backend with that env, streams
//! `AgentEvent`s to stdout/stderr, then bumps `last_used_at`.

use std::path::PathBuf;

use anatta_core::{AgentEvent, AgentEventPayload};
use anatta_runtime::profile::{
    providers, ClaudeProfile, ClaudeProfileId, CodexProfile, CodexProfileId, Overrides,
    ProviderEnv,
};
use anatta_runtime::spawn::{
    self, AgentSession, ClaudeLaunch, ClaudeSessionId, CodexLaunch, CodexThreadId, ExitInfo,
};
use anatta_runtime::{LockError, SessionLock};
use anatta_store::profile::{AuthMethod, BackendKind, ProfileRecord};
use clap::Args;

use crate::auth;
use crate::config::Config;

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
    #[error("backend binary `{0}` not found on PATH")]
    BinaryNotFound(&'static str),
    #[error("api key for profile {0} is missing from the OS keyring")]
    ApiKeyMissing(String),
    #[error(
        "provider `{0}` recorded on this profile is not in the runtime registry; \
         the binary may be older than the profile"
    )]
    UnknownProvider(String),
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

    #[error("auth: {0}")]
    Auth(#[from] auth::AuthError),
    #[error("profile: {0}")]
    Profile(#[from] anatta_runtime::profile::ProfileError),
    #[error("store: {0}")]
    Store(#[from] anatta_store::StoreError),
    #[error("spawn: {0}")]
    Spawn(#[from] spawn::SpawnError),
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

    match record.backend {
        BackendKind::Claude => {
            run_claude(args.prompt, args.resume, args.json, cwd, record, cfg).await
        }
        BackendKind::Codex => {
            run_codex(args.prompt, args.resume, args.json, cwd, record, cfg).await
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Claude
// ────────────────────────────────────────────────────────────────────────────

async fn run_claude(
    prompt: String,
    resume: Option<String>,
    json: bool,
    cwd: PathBuf,
    record: ProfileRecord,
    cfg: &Config,
) -> Result<(), SendError> {
    let lock = maybe_acquire_chat_lock(resume.as_deref(), cfg).await?;
    let api_key = resolve_api_key(&record)?;
    let launch = build_claude_launch(&record, api_key, prompt, resume, cwd, cfg)?;
    let session = spawn::launch(launch).await?;
    let exit = stream_session(session, json).await?;
    cfg.store.touch_profile(&record.id).await?;
    if let Some((name, _lock)) = lock {
        cfg.store.touch_conversation(&name).await?;
        // _lock drops here (flock released by OS).
    }
    enforce_exit("claude", exit)
}

/// Build a `ClaudeLaunch` from a stored profile + a pre-resolved
/// api key.
///
/// **The keychain read happens in [`resolve_api_key`], not here.** On
/// macOS each keychain access can prompt the user for a password;
/// callers that build many launches for the same profile (e.g.
/// `anatta chat` issuing one launch per turn) must call
/// `resolve_api_key` once outside the loop and pass the cached value
/// in.
pub(crate) fn build_claude_launch(
    record: &ProfileRecord,
    api_key: Option<String>,
    prompt: String,
    resume: Option<String>,
    cwd: PathBuf,
    cfg: &Config,
) -> Result<ClaudeLaunch, SendError> {
    let id = ClaudeProfileId::from_string(record.id.clone())?;
    let profile = ClaudeProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("claude").ok_or(SendError::BinaryNotFound("claude"))?;

    let provider = match (record.auth_method, api_key) {
        (AuthMethod::Login, _) => None,
        (AuthMethod::ApiKey, None) => return Err(SendError::ApiKeyMissing(record.id.clone())),
        (AuthMethod::ApiKey, Some(token)) => {
            let spec = providers::lookup(&record.provider)
                .ok_or_else(|| SendError::UnknownProvider(record.provider.clone()))?;
            let overrides = Overrides {
                base_url: record.base_url_override.clone(),
                model: record.model_override.clone(),
                small_fast_model: record.small_fast_model_override.clone(),
                default_opus_model: record.default_opus_model_override.clone(),
                default_sonnet_model: record.default_sonnet_model_override.clone(),
                default_haiku_model: record.default_haiku_model_override.clone(),
                subagent_model: record.subagent_model_override.clone(),
            };
            Some(ProviderEnv::build(spec, &overrides, token))
        }
    };

    Ok(ClaudeLaunch {
        profile,
        cwd,
        prompt,
        resume: resume.map(ClaudeSessionId::new),
        binary_path,
        provider,
    })
}

/// Read the profile's api key from the OS keyring exactly once.
///
/// Returns `Ok(None)` for `AuthMethod::Login` profiles (the backend
/// CLI does its own auth and no env injection is needed). For
/// `AuthMethod::ApiKey`, returns `Ok(Some(token))` or
/// `Err(ApiKeyMissing)` if the keyring has no entry.
///
/// **Callers that build many launches against the same profile must
/// cache this value** — every call triggers a keychain access which on
/// macOS can prompt the user for a password.
pub(crate) fn resolve_api_key(record: &ProfileRecord) -> Result<Option<String>, SendError> {
    match record.auth_method {
        AuthMethod::Login => Ok(None),
        AuthMethod::ApiKey => Ok(Some(
            auth::read_api_key(&record.id)?
                .ok_or_else(|| SendError::ApiKeyMissing(record.id.clone()))?,
        )),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Codex
// ────────────────────────────────────────────────────────────────────────────

async fn run_codex(
    prompt: String,
    resume: Option<String>,
    json: bool,
    cwd: PathBuf,
    record: ProfileRecord,
    cfg: &Config,
) -> Result<(), SendError> {
    let lock = maybe_acquire_chat_lock(resume.as_deref(), cfg).await?;
    let api_key = resolve_api_key(&record)?;
    let launch = build_codex_launch(&record, api_key, prompt, resume, cwd, cfg)?;
    let session = spawn::launch(launch).await?;
    let exit = stream_session(session, json).await?;
    cfg.store.touch_profile(&record.id).await?;
    if let Some((name, _lock)) = lock {
        cfg.store.touch_conversation(&name).await?;
    }
    enforce_exit("codex", exit)
}

/// If `--resume <id>` targets a backend_session_id recorded against a
/// named conversation, acquire its [`SessionLock`] so a concurrent
/// `anatta chat resume` is blocked. If no matching conversation
/// exists, return None (ad-hoc resume against an arbitrary id is
/// allowed without coordination).
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

/// Build a `CodexLaunch` from a stored profile + a pre-resolved api
/// key. See [`resolve_api_key`] re: caching across many calls.
pub(crate) fn build_codex_launch(
    record: &ProfileRecord,
    api_key: Option<String>,
    prompt: String,
    resume: Option<String>,
    cwd: PathBuf,
    cfg: &Config,
) -> Result<CodexLaunch, SendError> {
    let id = CodexProfileId::from_string(record.id.clone())?;
    let profile = CodexProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("codex").ok_or(SendError::BinaryNotFound("codex"))?;

    // For login profiles `api_key` is `None`; codex finds its own
    // creds via `CODEX_HOME/auth.json`. For api-key profiles, the
    // caller is expected to have resolved a Some(_) — we treat None
    // here as ApiKeyMissing to match the claude path.
    let api_key = match record.auth_method {
        AuthMethod::Login => None,
        AuthMethod::ApiKey => match api_key {
            Some(k) => Some(k),
            None => return Err(SendError::ApiKeyMissing(record.id.clone())),
        },
    };

    Ok(CodexLaunch {
        profile,
        cwd,
        prompt,
        resume: resume.map(CodexThreadId::new),
        binary_path,
        api_key,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Streaming
// ────────────────────────────────────────────────────────────────────────────

async fn stream_session(mut session: AgentSession, json: bool) -> Result<ExitInfo, SendError> {
    eprintln!("[anatta] session: {}", session.session_id());
    while let Some(ev) = session.events().recv().await {
        if json {
            println!("{}", serde_json::to_string(&ev)?);
        } else {
            render_pretty(&ev);
        }
    }
    let exit = session.wait().await?;
    eprintln!(
        "[anatta] exit: code={:?} signal={:?} duration={:?} events={}",
        exit.exit_code, exit.signal, exit.duration, exit.events_emitted,
    );
    Ok(exit)
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
        RateLimit { limit_kind, .. } => eprintln!("[rate-limit] {limit_kind}"),
        Error { message, fatal } => {
            eprintln!("[error{}] {message}", if *fatal { " fatal" } else { "" });
        }
        TurnStarted | UserPrompt { .. } => {}
        // Snapshots — the finalized variants carry the same content. Skip.
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
