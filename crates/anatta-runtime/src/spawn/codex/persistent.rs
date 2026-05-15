//! Long-lived `codex app-server` connection shared across many turns.
//!
//! Construction ([`PersistentCodexSession::open`]) runs the initialize /
//! thread/start handshake once. After that, [`send_turn`](Self::send_turn)
//! issues a `turn/start` request and returns a [`TurnHandle`] whose
//! event channel drains until the turn's `turn/completed` notification.
//!
//! Cancellation is per-turn via [`CodexInterruptHandle`] (obtained from
//! [`interrupt_handle`](PersistentCodexSession::interrupt_handle)): it
//! sends `turn/interrupt`; codex emits `turn/completed { status:
//! "interrupted" }` shortly after, closing the handle's channel
//! naturally — the session itself stays open.
//!
//! Closing the session ([`close`](Self::close)) writes EOF on stdin so
//! codex shuts down cleanly and the child exits with status 0.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anatta_core::AgentEvent;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc};

use crate::codex::app_server::AppServerProjector;
use crate::codex::app_server::wire::{TurnInput, TurnStartParams};
use crate::spawn::{ExitInfo, SpawnError, stderr_buf};

use super::handshake::{Handshake, handshake};
use super::launch::CodexLaunch;
use super::pump::{persistent_reader_loop, push_synthetic_session_started, write_request};
use super::FIRST_TURN_REQUEST_ID;

pub struct PersistentCodexSession {
    child: Child,
    /// Stdin protected by a mutex because `send_turn` and
    /// `interrupt_current_turn` may both write at non-overlapping but
    /// possibly concurrent moments.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    thread_id: String,
    cwd_str: String,
    next_request_id: Arc<AtomicI64>,
    /// The currently-active turn's state, or None when idle.
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    stderr: stderr_buf::Handle,
    started_at: Instant,
    events_emitted: Arc<AtomicU64>,
    /// Per-turn policy derived from the session's current permission
    /// level. Initialized in open(); mutated in set_permission_level
    /// (Task 9).
    current_policy: Mutex<anatta_core::CodexPolicy>,
}

/// Clonable handle for interrupting the currently-active turn of a
/// [`PersistentCodexSession`] from outside the session struct itself.
///
/// Created via [`PersistentCodexSession::interrupt_handle`]. All fields
/// are cheap-to-clone (Arc / String). The handle keeps the inner
/// state alive only as long as needed — if the session is dropped, the
/// stdin/active_turn it points to become inert (subsequent
/// `interrupt()` calls find no active turn and no-op).
#[derive(Clone)]
pub struct CodexInterruptHandle {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    thread_id: String,
    next_request_id: Arc<AtomicI64>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
}

impl CodexInterruptHandle {
    /// Send `turn/interrupt` for the currently-active turn. No-op if no
    /// turn is active or the session is closed. See
    /// [`PersistentCodexSession::interrupt_current_turn`].
    pub async fn interrupt(&self) -> Result<(), SpawnError> {
        let turn_id = {
            let active = self.active_turn.lock().await;
            match &*active {
                Some(t) => t.turn_id.clone(),
                None => return Ok(()),
            }
        };
        let Some(tid) = turn_id else {
            return Ok(());
        };
        let req_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let mut stdin_guard = self.stdin.lock().await;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Ok(()); // session closed
        };
        write_request(
            stdin,
            req_id,
            "turn/interrupt",
            serde_json::json!({ "threadId": &self.thread_id, "turnId": tid }),
        )
        .await
    }
}

pub(super) struct ActiveTurn {
    /// JSON-RPC request id we used for this turn's `turn/start`.
    /// An error response on this id surfaces as an Error event +
    /// channel close.
    pub request_id: i64,
    /// Filled in when we observe `turn/started` (carries `turn.id`).
    /// Needed for `turn/interrupt`.
    pub turn_id: Option<String>,
    pub events_tx: mpsc::Sender<AgentEvent>,
    pub projector: AppServerProjector,
}

/// Per-turn handle returned by [`PersistentCodexSession::send_turn`].
///
/// Drain `events()` until it returns `None`; that signals the turn
/// ended (naturally via `turn/completed`, by interrupt, or by an
/// error response).
pub struct TurnHandle {
    events_rx: mpsc::Receiver<AgentEvent>,
}

impl TurnHandle {
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events_rx
    }
}

impl PersistentCodexSession {
    pub async fn open(launch: CodexLaunch) -> Result<Self, SpawnError> {
        let started_at = Instant::now();
        let initial_policy = launch.permission_level.codex_policy();
        let Handshake {
            child,
            stdin,
            reader,
            stderr,
            thread_id,
            cwd_str,
        } = handshake(
            &launch.binary_path,
            &launch.profile,
            &launch.cwd,
            launch.api_key.as_deref(),
            launch.resume.as_ref().map(|r| r.as_str()),
            initial_policy,
        )
        .await?;

        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let active_turn: Arc<Mutex<Option<ActiveTurn>>> = Arc::new(Mutex::new(None));
        let events_emitted = Arc::new(AtomicU64::new(0));

        // Background reader: forwards notifications to the currently-
        // active turn's events_tx; routes error responses for the
        // active turn's request id to an Error event + close.
        let reader_active = active_turn.clone();
        let reader_counter = events_emitted.clone();
        let reader_stderr = stderr.clone();
        let reader_thread_id = thread_id.clone();
        tokio::spawn(async move {
            persistent_reader_loop(
                reader,
                reader_active,
                reader_counter,
                reader_stderr,
                reader_thread_id,
            )
            .await;
        });

        Ok(Self {
            child,
            stdin,
            thread_id,
            cwd_str,
            next_request_id: Arc::new(AtomicI64::new(FIRST_TURN_REQUEST_ID)),
            active_turn,
            stderr,
            started_at,
            events_emitted,
            current_policy: Mutex::new(initial_policy),
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// True iff no turn is currently in progress. Used by
    /// [`crate::spawn::Session::is_idle`] (cross-engine swap precondition).
    pub async fn is_idle(&self) -> bool {
        self.active_turn.lock().await.is_none()
    }

    /// Clone-friendly handle for interrupting the active turn from
    /// outside (e.g., a [`TurnEvents`](crate::spawn::TurnEvents)
    /// wrapper that owns the channel but not the session).
    pub fn interrupt_handle(&self) -> CodexInterruptHandle {
        CodexInterruptHandle {
            stdin: self.stdin.clone(),
            thread_id: self.thread_id.clone(),
            next_request_id: self.next_request_id.clone(),
            active_turn: self.active_turn.clone(),
        }
    }

    /// Start a new turn. Refuses if a previous turn is still active.
    pub async fn send_turn(&self, prompt: &str) -> Result<TurnHandle, SpawnError> {
        // Reserve the request id BEFORE installing the active turn so
        // the reader can match errors to it.
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);

        {
            let mut active = self.active_turn.lock().await;
            if active.is_some() {
                return Err(SpawnError::Io(std::io::Error::other(
                    "send_turn called while previous turn still active",
                )));
            }
            *active = Some(ActiveTurn {
                request_id,
                turn_id: None,
                events_tx: events_tx.clone(),
                projector: AppServerProjector::new(self.thread_id.clone()),
            });
        }

        // Emit a synthetic SessionStarted as the first event on every
        // turn, mirroring the one-shot path. Renderers can no-op it.
        push_synthetic_session_started(
            &events_tx,
            &self.events_emitted,
            &self.thread_id,
            &self.cwd_str,
        )
        .await?;

        // Snapshot the current policy before acquiring stdin so we
        // don't hold both locks across the .await.
        let approval = {
            let policy = self.current_policy.lock().await;
            policy.approval
        };

        // Write turn/start.
        let mut stdin_guard = self.stdin.lock().await;
        let stdin = stdin_guard
            .as_mut()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("session already closed")))?;
        write_request(
            stdin,
            request_id,
            "turn/start",
            TurnStartParams {
                thread_id: &self.thread_id,
                input: vec![TurnInput::Text { text: prompt }],
                approval_policy: approval,
                cwd: &self.cwd_str,
            },
        )
        .await?;

        Ok(TurnHandle { events_rx })
    }

    /// Close the session: drop stdin (graceful EOF → codex exits),
    /// wait for the child, return the final ExitInfo.
    pub async fn close(mut self) -> Result<ExitInfo, SpawnError> {
        {
            // Drop stdin to signal shutdown.
            let mut guard = self.stdin.lock().await;
            guard.take();
        }
        // Race child exit against a short grace; SIGKILL on timeout
        // (defensive — codex always exits cleanly on EOF in practice).
        let status = match tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await {
            Ok(res) => res.map_err(SpawnError::Io)?,
            Err(_) => {
                let _ = self.child.start_kill();
                self.child.wait().await.map_err(SpawnError::Io)?
            }
        };
        Ok(ExitInfo {
            exit_code: status.code(),
            #[cfg(unix)]
            signal: std::os::unix::process::ExitStatusExt::signal(&status),
            #[cfg(not(unix))]
            signal: None,
            duration: self.started_at.elapsed(),
            stderr_tail: self.stderr.snapshot(),
            events_emitted: self.events_emitted.load(Ordering::Relaxed),
        })
    }
}
