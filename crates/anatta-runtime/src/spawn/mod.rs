//! Backend subprocess supervision.
//!
//! Each backend exposes a typed `XxxLaunch` config + an impl of
//! [`Launchable`]. Top-level [`launch`] is a thin wrapper that delegates
//! to the trait method.
//!
//! ```ignore
//! use anatta_runtime::{claude, spawn::launch};
//! let session = launch(claude::ClaudeLaunch {
//!     profile: claude_profile,
//!     cwd: worktree.into(),
//!     prompt: "do the thing".into(),
//!     resume: None,
//!     binary_path: claude_bin,
//! }).await?;
//! let id = session.session_id();
//! while let Some(evt) = session.events().recv().await { ... }
//! ```
//!
//! `launch()` blocks until the first event arrives, extracting
//! `session_id` from it (claude's `system/init`, codex's
//! `thread.started`). The first event is also forwarded to the
//! consumer-facing channel — nothing is silently consumed.

mod claude;
mod claude_interactive;
mod codex;
mod ids;
pub(crate) mod pipeline;
mod session;
mod stderr_buf;

pub use claude::ClaudeLaunch;
pub use claude_interactive::{
    encode_prompt_for_test, run_tail_for_test, ClaudeInteractiveInterruptHandle,
    ClaudeInteractiveLaunch, ClaudeInteractiveSession, InteractiveTurnHandle,
};
pub use codex::{CodexInterruptHandle, CodexLaunch, PersistentCodexSession, TurnHandle};
pub use ids::{ClaudeSessionId, CodexThreadId};
pub use session::{
    BackendKind, BackendLaunch, ClaudeSession, CodexSession, Session, SwapError, TurnEvents,
};

use std::path::PathBuf;
use std::time::Duration;

use anatta_core::AgentEvent;
use async_trait::async_trait;
use tokio::process::Child;
use tokio::sync::mpsc;

/// Lossless launch contract. Each backend's typed launch config implements
/// this; [`launch`] is the top-level convenience wrapper.
#[async_trait]
pub trait Launchable {
    async fn launch(self) -> Result<AgentSession, SpawnError>;
}

/// Top-level discoverability wrapper. Equivalent to `cfg.launch().await`.
pub async fn launch<L: Launchable + Send>(cfg: L) -> Result<AgentSession, SpawnError> {
    cfg.launch().await
}

/// A live backend session. Holds the child process plus a receive end
/// of the projected `AgentEvent` stream. Drop will SIGKILL the child
/// (via tokio's `kill_on_drop`) if [`cancel`](Self::cancel) /
/// [`wait`](Self::wait) wasn't called first.
#[derive(Debug)]
pub struct AgentSession {
    session_id: String,
    child: Child,
    events_rx: mpsc::Receiver<AgentEvent>,
    stderr: stderr_buf::Handle,
    started_at: std::time::Instant,
    events_emitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl AgentSession {
    /// Direct constructor for backends that don't go through
    /// `finalize_first_event_session` (e.g. codex app-server, which
    /// captures the thread id from a JSON-RPC response rather than a
    /// first event).
    pub(crate) fn new(
        session_id: String,
        child: Child,
        events_rx: mpsc::Receiver<AgentEvent>,
        stderr: stderr_buf::Handle,
        started_at: std::time::Instant,
        events_emitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            session_id,
            child,
            events_rx,
            stderr,
            started_at,
            events_emitted,
        }
    }

    /// session_id (claude session UUID or codex thread UUID), known
    /// from the first event.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Mutable reference to the projected event stream. Drain this until
    /// it returns `None` (channel closed = backend exited).
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events_rx
    }

    /// Wait for the backend to exit naturally. Returns final ExitInfo.
    pub async fn wait(mut self) -> Result<ExitInfo, SpawnError> {
        let status = self.child.wait().await.map_err(SpawnError::Io)?;
        Ok(self.build_exit_info(status))
    }

    /// Graceful cancel: drop stdin so the backend sees EOF, wait
    /// `grace_period` for natural exit, then SIGKILL via tokio.
    pub async fn cancel(self) -> Result<ExitInfo, SpawnError> {
        self.cancel_with_timeout(Duration::from_secs(3)).await
    }

    /// Cancel with explicit grace timeout.
    pub async fn cancel_with_timeout(
        mut self,
        grace_period: Duration,
    ) -> Result<ExitInfo, SpawnError> {
        self.cancel_mut_with_timeout(grace_period).await
    }

    /// Non-consuming cancel variant for callers that need to keep
    /// the `AgentSession` value around (e.g., to drain remaining
    /// events from the receiver after the child exits, or to call
    /// `wait` from a different branch of a `select!`).
    ///
    /// Uses the same 3-second default grace period as [`cancel`].
    pub async fn cancel_mut(&mut self) -> Result<ExitInfo, SpawnError> {
        self.cancel_mut_with_timeout(Duration::from_secs(3)).await
    }

    /// Non-consuming cancel with explicit grace timeout.
    pub async fn cancel_mut_with_timeout(
        &mut self,
        grace_period: Duration,
    ) -> Result<ExitInfo, SpawnError> {
        // 1. Close stdin (drop) so the backend sees EOF.
        drop(self.child.stdin.take());
        // 2. Race the natural exit against the grace timer.
        let status = match tokio::time::timeout(grace_period, self.child.wait()).await {
            Ok(res) => res.map_err(SpawnError::Io)?,
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await.map_err(SpawnError::Io)?
            }
        };
        Ok(self.build_exit_info(status))
    }

    fn build_exit_info(&mut self, status: std::process::ExitStatus) -> ExitInfo {
        ExitInfo {
            exit_code: status.code(),
            #[cfg(unix)]
            signal: std::os::unix::process::ExitStatusExt::signal(&status),
            #[cfg(not(unix))]
            signal: None,
            duration: self.started_at.elapsed(),
            stderr_tail: self.stderr.snapshot(),
            events_emitted: self
                .events_emitted
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Final state of a backend process after exit / cancel.
#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration: Duration,
    /// Last ~64 KB of the backend's stderr.
    pub stderr_tail: String,
    pub events_emitted: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("backend binary not found at {0}")]
    BinaryNotFound(PathBuf),
    #[error("profile path is not a directory: {0}")]
    ProfilePathInvalid(PathBuf),
    #[error("failed to spawn child process: {0}")]
    ProcessSpawn(#[source] std::io::Error),
    #[error("child exited before emitting any event (status={status:?}); stderr tail: {stderr_tail}")]
    ChildExitedEarly {
        status: Option<i32>,
        stderr_tail: String,
    },
    #[error("first event from backend did not parse as expected: {0}")]
    ParseFirstEvent(#[source] serde_json::Error),
    #[error("first event was {got:?}, expected {expected}")]
    UnexpectedFirstEvent {
        expected: &'static str,
        got: String,
    },
    #[error("first event carried no session_id (or thread_id)")]
    MissingSessionId,
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
}

// ────────────────────────────────────────────────────────────────────────────
// Shared post-spawn finalization
// ────────────────────────────────────────────────────────────────────────────

/// Wait for the first AgentEvent off the pipeline, extract session_id
/// from its envelope, then re-publish it (and forward all subsequent
/// events) to a new consumer-facing channel that backs the
/// AgentSession. Used identically by claude and codex launches because
/// both backends emit a session-identifying first event (system/init
/// for claude, thread.started for codex), which our projectors already
/// stamp into AgentEvent envelope.session_id.
async fn finalize_first_event_session(
    mut handles: pipeline::PipelineHandles,
) -> Result<AgentSession, SpawnError> {
    let started_at = std::time::Instant::now();
    let first = handles.events_rx.recv().await.ok_or_else(|| {
        SpawnError::ChildExitedEarly {
            status: None,
            stderr_tail: handles.stderr.snapshot(),
        }
    })?;
    if first.envelope.session_id.is_empty() {
        return Err(SpawnError::MissingSessionId);
    }
    let session_id = first.envelope.session_id.clone();

    let (consumer_tx, consumer_rx) = mpsc::channel::<AgentEvent>(64);
    consumer_tx
        .send(first)
        .await
        .map_err(|_| SpawnError::Io(std::io::Error::other("consumer channel closed during init")))?;
    tokio::spawn(async move {
        while let Some(e) = handles.events_rx.recv().await {
            if consumer_tx.send(e).await.is_err() {
                break;
            }
        }
    });

    Ok(AgentSession {
        session_id,
        child: handles.child,
        events_rx: consumer_rx,
        stderr: handles.stderr,
        started_at,
        events_emitted: handles.events_emitted,
    })
}
