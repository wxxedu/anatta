//! Backend-agnostic session abstraction.
//!
//! [`Session`] hides the difference between claude (per-turn `claude -p
//! --resume` spawn) and codex (persistent `app-server` over JSON-RPC).
//! Consumers — the chat REPL, the one-shot `send` command, and the
//! future daemon — drive a session with the same calls:
//!
//! ```ignore
//! let mut session = Session::open(launch).await?;
//! loop {
//!     let mut turn = session.send_turn(&prompt).await?;
//!     while let Some(ev) = turn.recv().await { render(ev) }
//!     let _ = turn.finalize().await?;
//! }
//! let _ = session.close().await?;
//! ```
//!
//! Profile swap (`/profile` in chat) calls [`Session::swap`] with a new
//! same-backend [`BackendLaunch`]; cross-backend swap is rejected at the
//! seam (see [`SwapError::BackendMismatch`]).
//!
//! Per-turn cancellation goes through [`TurnEvents::cancel`]. For claude
//! that closes stdin + SIGKILLs the per-turn child within a grace
//! window; for codex it sends `turn/interrupt`, after which codex emits
//! `turn/completed { status: "interrupted" }` and the channel closes.

use anatta_core::AgentEvent;

use super::{
    AgentSession, ClaudeLaunch, ClaudeSessionId, CodexInterruptHandle, CodexLaunch, CodexThreadId,
    ExitInfo, Launchable, PersistentCodexSession, SpawnError, TurnHandle,
};

/// Which backend is on the other end. Mirrors the store's `BackendKind`
/// but kept local to runtime so this crate stays storage-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Claude,
    Codex,
}

/// Fully-resolved description of how to start (or continue) a backend
/// session. The CLI builds one of these from a stored profile +
/// credentials; the runtime takes it from there.
///
/// Both variants carry a `prompt` field for symmetry with the one-shot
/// [`Launchable`] contract. For chat use, leave `prompt` empty — it is
/// only consumed by the codex one-shot flow and otherwise ignored.
#[derive(Debug, Clone)]
pub enum BackendLaunch {
    Claude(ClaudeLaunch),
    Codex(CodexLaunch),
}

impl BackendLaunch {
    pub fn kind(&self) -> BackendKind {
        match self {
            BackendLaunch::Claude(_) => BackendKind::Claude,
            BackendLaunch::Codex(_) => BackendKind::Codex,
        }
    }

    /// cwd the backend will run in.
    pub fn cwd(&self) -> &std::path::Path {
        match self {
            BackendLaunch::Claude(l) => &l.cwd,
            BackendLaunch::Codex(l) => &l.cwd,
        }
    }

    /// Resume id, if any (claude session UUID / codex thread UUID).
    pub fn resume_id(&self) -> Option<&str> {
        match self {
            BackendLaunch::Claude(l) => l.resume.as_ref().map(|r| r.as_str()),
            BackendLaunch::Codex(l) => l.resume.as_ref().map(|r| r.as_str()),
        }
    }
}

/// A live backend session that can drive many turns. Claude is
/// stateless per turn (each [`send_turn`](Self::send_turn) spawns a
/// fresh child); codex is a persistent `app-server` process whose
/// lifetime spans the whole session.
pub enum Session {
    Claude(ClaudeSession),
    Codex(CodexSession),
}

impl Session {
    pub async fn open(launch: BackendLaunch) -> Result<Self, SpawnError> {
        match launch {
            BackendLaunch::Claude(l) => Ok(Session::Claude(ClaudeSession::open(l))),
            BackendLaunch::Codex(l) => Ok(Session::Codex(CodexSession::open(l).await?)),
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Session::Claude(_) => BackendKind::Claude,
            Session::Codex(_) => BackendKind::Codex,
        }
    }

    /// True iff no turn is currently running. Claude is per-turn-spawn —
    /// the session has no inter-turn live state, so it is always idle
    /// when control returns to the chat loop. Codex is a long-lived
    /// app-server; idleness depends on whether a turn is in flight.
    pub async fn is_idle(&self) -> bool {
        match self {
            Session::Claude(_) => true,
            Session::Codex(c) => c.inner.is_idle().await,
        }
    }

    /// Claude session UUID or codex thread UUID. For claude this is
    /// `None` until the first turn has produced a `system/init`.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Session::Claude(c) => c.thread_id.as_ref().map(|t| t.as_str()),
            Session::Codex(c) => Some(c.inner.thread_id()),
        }
    }

    /// Send a new turn. Returns a [`TurnEvents`] whose channel closes
    /// when the turn finishes (naturally, by interrupt, or by error).
    pub async fn send_turn(&mut self, prompt: &str) -> Result<TurnEvents, SpawnError> {
        match self {
            Session::Claude(c) => c.send_turn(prompt).await,
            Session::Codex(c) => c.send_turn(prompt).await,
        }
    }

    /// Reconfigure for a new same-backend launch (typically `/profile`
    /// in chat: different auth / env / model overrides, same thread).
    /// Cross-backend swap is rejected.
    pub async fn swap(&mut self, new_launch: BackendLaunch) -> Result<(), SwapError> {
        match (self, new_launch) {
            (Session::Claude(c), BackendLaunch::Claude(l)) => {
                c.swap(l);
                Ok(())
            }
            (Session::Codex(c), BackendLaunch::Codex(l)) => {
                c.swap(l).await.map_err(SwapError::Spawn)
            }
            (s, l) => Err(SwapError::BackendMismatch {
                current: s.kind(),
                target: l.kind(),
            }),
        }
    }

    /// Close the session and return final exit info if the backend has
    /// a persistent process to harvest (codex). For claude there is no
    /// session-level process; returns `None`.
    pub async fn close(self) -> Result<Option<ExitInfo>, SpawnError> {
        match self {
            Session::Claude(_) => Ok(None),
            Session::Codex(c) => Ok(Some(c.close().await?)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error(
        "cross-backend swap not supported (current: {current:?}, target: {target:?})"
    )]
    BackendMismatch {
        current: BackendKind,
        target: BackendKind,
    },
    #[error(transparent)]
    Spawn(#[from] SpawnError),
}

// ──────────────────────────────────────────────────────────────────────
// Claude
// ──────────────────────────────────────────────────────────────────────

/// Per-turn-spawn claude session. Each [`send_turn`](Self::send_turn)
/// clones the template, fills in prompt + resume id, and spawns a fresh
/// `claude --print --output-format stream-json` child.
pub struct ClaudeSession {
    /// Template launch (prompt/resume are overridden per turn).
    template: ClaudeLaunch,
    /// Captured after the first turn's `system/init`. Used for
    /// `--resume <id>` on subsequent turns to keep history continuous.
    thread_id: Option<ClaudeSessionId>,
}

impl ClaudeSession {
    fn open(template: ClaudeLaunch) -> Self {
        let thread_id = template.resume.clone();
        Self {
            template,
            thread_id,
        }
    }

    async fn send_turn(&mut self, prompt: &str) -> Result<TurnEvents, SpawnError> {
        let mut launch = self.template.clone();
        launch.prompt = prompt.to_owned();
        launch.resume = self.thread_id.clone();
        let session = launch.launch().await?;
        if self.thread_id.is_none() {
            self.thread_id = Some(ClaudeSessionId::new(session.session_id().to_owned()));
        }
        Ok(TurnEvents {
            inner: TurnEventsInner::Claude(session),
            captured_exit: None,
        })
    }

    fn swap(&mut self, new_template: ClaudeLaunch) {
        // Keep the running thread id; only auth/env/model overrides
        // change. (The new template's `resume` field is overwritten
        // per turn from `self.thread_id`.)
        self.template = new_template;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Codex
// ──────────────────────────────────────────────────────────────────────

/// Persistent `codex app-server` session.
pub struct CodexSession {
    inner: PersistentCodexSession,
}

impl CodexSession {
    async fn open(launch: CodexLaunch) -> Result<Self, SpawnError> {
        let inner = PersistentCodexSession::open(launch).await?;
        Ok(Self { inner })
    }

    async fn send_turn(&mut self, prompt: &str) -> Result<TurnEvents, SpawnError> {
        let handle = self.inner.send_turn(prompt).await?;
        let interrupt = self.inner.interrupt_handle();
        Ok(TurnEvents {
            inner: TurnEventsInner::Codex { handle, interrupt },
            captured_exit: None,
        })
    }

    async fn swap(&mut self, mut new_launch: CodexLaunch) -> Result<(), SpawnError> {
        // Re-open against the same thread id so history is preserved;
        // only the new launch's auth/env/binary differ. Order: open new
        // first, then close old — if the new one fails we keep the old
        // session intact instead of leaving the caller with nothing.
        let thread_id = self.inner.thread_id().to_owned();
        new_launch.resume = Some(CodexThreadId::new(thread_id));
        let new_inner = PersistentCodexSession::open(new_launch).await?;
        let old = std::mem::replace(&mut self.inner, new_inner);
        let _ = old.close().await;
        Ok(())
    }

    async fn close(self) -> Result<ExitInfo, SpawnError> {
        self.inner.close().await
    }
}

// ──────────────────────────────────────────────────────────────────────
// TurnEvents
// ──────────────────────────────────────────────────────────────────────

/// One turn's event stream. Drain via [`recv`](Self::recv) until it
/// returns `None`. Use [`cancel`](Self::cancel) to interrupt mid-turn
/// (Ctrl-C). After the channel closes, [`finalize`](Self::finalize)
/// returns per-turn exit info for backends that produce it (claude) or
/// `None` (codex; session-level exit is on [`Session::close`]).
pub struct TurnEvents {
    inner: TurnEventsInner,
    /// Populated by `cancel()` on claude (which consumes the exit
    /// status when it kills the child) so `finalize()` doesn't try to
    /// re-`wait` the already-reaped child.
    captured_exit: Option<ExitInfo>,
}

enum TurnEventsInner {
    Claude(AgentSession),
    Codex {
        handle: TurnHandle,
        interrupt: CodexInterruptHandle,
    },
}

impl TurnEvents {
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        match &mut self.inner {
            TurnEventsInner::Claude(s) => s.events().recv().await,
            TurnEventsInner::Codex { handle, .. } => handle.events().recv().await,
        }
    }

    /// Interrupt the turn. Idempotent (calls after the first are
    /// no-ops). For claude, drops stdin and SIGKILLs within a 3s
    /// grace window; the event channel closes shortly after. For
    /// codex, sends `turn/interrupt`; codex emits `turn/completed
    /// { status: "interrupted" }` which closes the channel naturally
    /// while the underlying session stays alive.
    pub async fn cancel(&mut self) -> Result<(), SpawnError> {
        if self.captured_exit.is_some() {
            return Ok(());
        }
        match &mut self.inner {
            TurnEventsInner::Claude(s) => {
                let exit = s.cancel_mut().await?;
                self.captured_exit = Some(exit);
                Ok(())
            }
            TurnEventsInner::Codex { interrupt, .. } => interrupt.interrupt().await,
        }
    }

    /// Finalize after the channel has closed. Returns per-turn exit
    /// info for claude (consumed from the per-turn child) or `None`
    /// for codex (session-level exit lives on [`Session::close`]).
    pub async fn finalize(mut self) -> Result<Option<ExitInfo>, SpawnError> {
        if let Some(exit) = self.captured_exit.take() {
            return Ok(Some(exit));
        }
        match self.inner {
            TurnEventsInner::Claude(s) => Ok(Some(s.wait().await?)),
            TurnEventsInner::Codex { .. } => Ok(None),
        }
    }
}

