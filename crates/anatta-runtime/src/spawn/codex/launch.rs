//! One-shot `codex app-server` launch.
//!
//! Implements [`Launchable`] for [`CodexLaunch`]. The flow:
//!
//!   1. Handshake (initialize + thread/start or thread/resume).
//!   2. `turn/start` with the user's prompt.
//!   3. Background pump task drains notifications into AgentEvents
//!      and closes stdin on `turn/completed` so the app-server exits.
//!
//! Returns an [`AgentSession`] consistent with claude's per-turn-spawn
//! contract.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use anatta_core::AgentEvent;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::codex::app_server::AppServerProjector;
use crate::codex::app_server::wire::{TurnInput, TurnStartParams};
use crate::profile::CodexProfile;
use crate::spawn::{AgentSession, CodexThreadId, Launchable, SpawnError};

use super::FIRST_TURN_REQUEST_ID;
use super::handshake::{Handshake, handshake};
use super::pump::{make_error_event, push_synthetic_session_started, run_pump, write_request};

/// Configuration for spawning a codex session.
#[derive(Debug, Clone)]
pub struct CodexLaunch {
    pub profile: CodexProfile,
    pub cwd: PathBuf,
    pub prompt: String,
    /// `Some(id)` → `thread/resume <id>`. `None` → fresh `thread/start`.
    pub resume: Option<CodexThreadId>,
    pub binary_path: PathBuf,
    /// `Some(key)` → set `OPENAI_API_KEY` on the spawned process.
    /// `None` → codex finds its own auth via `CODEX_HOME/auth.json`.
    pub api_key: Option<String>,
    /// Initial permission level. The mapping to codex's two per-turn
    /// axes (approval_policy, sandbox) and the session-level
    /// `approvals_reviewer` is in `PermissionLevel::codex_policy`.
    pub permission_level: anatta_core::PermissionLevel,
}

#[async_trait]
impl Launchable for CodexLaunch {
    async fn launch(self) -> Result<AgentSession, SpawnError> {
        let started_at = Instant::now();
        let policy = self.permission_level.codex_policy();
        let Handshake {
            child,
            mut stdin,
            mut reader,
            stderr,
            thread_id,
            cwd_str,
        } = handshake(
            &self.binary_path,
            &self.profile,
            &self.cwd,
            self.api_key.as_deref(),
            self.resume.as_ref().map(|r| r.as_str()),
            policy,
        )
        .await?;

        // turn/start with the user's prompt.
        write_request(
            &mut stdin,
            FIRST_TURN_REQUEST_ID,
            "turn/start",
            TurnStartParams {
                thread_id: &thread_id,
                input: vec![TurnInput::Text { text: &self.prompt }],
                approval_policy: policy.approval,
                cwd: &cwd_str,
            },
        )
        .await?;

        // Spawn the notification pump. Emits AgentEvents into mpsc;
        // on turn/completed it closes stdin so codex exits cleanly.
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);
        let counter = Arc::new(AtomicU64::new(0));

        // Synthetic SessionStarted (claude does the same via its
        // system/init forwarding — keeps the consumer contract uniform).
        push_synthetic_session_started(&events_tx, &counter, &thread_id, &cwd_str).await?;

        let pump_session_id = thread_id.clone();
        let counter_for_task = counter.clone();
        let stderr_for_pump = stderr.clone();
        tokio::spawn(async move {
            let mut projector = AppServerProjector::new(pump_session_id);
            let mut stdin_holder = Some(stdin);
            run_pump(
                &mut reader,
                &mut projector,
                &events_tx,
                &counter_for_task,
                &stderr_for_pump,
                |method| method == "turn/completed",
                make_error_event,
                &mut stdin_holder,
            )
            .await;
        });

        Ok(AgentSession::new(
            thread_id, child, events_rx, stderr, started_at, counter,
        ))
    }
}
