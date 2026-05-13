//! Codex `app-server` (JSON-RPC 2.0 over stdio) launch.
//!
//! Two consumers:
//!
//! * `anatta send --resume <id>` (one-shot, single turn):
//!   `CodexLaunch::launch()` → `AgentSession`. Spawns app-server,
//!   handshakes, sends `turn/start`, drains, closes. Matches the
//!   `Launchable` contract used by all backends.
//!
//! * `anatta chat` (long-lived, many turns):
//!   `PersistentCodexSession::open()` → instance with `send_turn(prompt)`,
//!   plus a clone-friendly [`CodexInterruptHandle`] (via
//!   [`PersistentCodexSession::interrupt_handle`]) for cancelling the
//!   active turn from outside. The codex app-server stays alive for the
//!   entire chat session; each turn is just a `turn/start` request,
//!   eliminating the ~200-500ms handshake-per-turn cost and matching
//!   codex's intended client model (the VS Code extension does the same).
//!
//! Per-turn protocol (one-shot mode):
//!
//! ```text
//!   anatta                                 codex app-server
//!     --- initialize ---------------------------> id=0
//!     <--- initialize result --------------------
//!     --- initialized (notification) ----------->
//!     --- thread/start or thread/resume --------> id=1
//!     <--- thread/start result (thread.id) ------
//!     --- turn/start (prompt) ------------------> id=2
//!     <--- turn/start result --------------------
//!     [<--- turn/started ... turn/completed ----] (notifications)
//!     --- (close stdin) ------------------------>  graceful shutdown
//! ```
//!
//! In persistent mode the first three steps run at `open()`; each
//! subsequent `send_turn` only does `turn/start` + drain.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anatta_core::AgentEvent;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};

use crate::codex::app_server::wire::{
    ClientInfo, InitializeParams, InitializedParams, OutgoingNotification, OutgoingRequest,
    ThreadResumeParams, ThreadStartParams, TurnInput, TurnStartParams,
};
use crate::codex::app_server::AppServerProjector;
use crate::profile::CodexProfile;

use super::stderr_buf;
use super::{AgentSession, CodexThreadId, ExitInfo, Launchable, SpawnError};

const APPROVAL_POLICY: &str = "never";
const SANDBOX_POLICY: &str = "danger-full-access";

const INITIALIZE_REQUEST_ID: i64 = 0;
const THREAD_REQUEST_ID: i64 = 1;
/// One-shot launch sends turn/start once at this id. Persistent
/// sessions allocate ids monotonically starting from this value.
const FIRST_TURN_REQUEST_ID: i64 = 2;

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
}

#[async_trait]
impl Launchable for CodexLaunch {
    async fn launch(self) -> Result<AgentSession, SpawnError> {
        let started_at = Instant::now();
        let Handshake {
            mut child,
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
                approval_policy: APPROVAL_POLICY,
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
        push_synthetic_session_started(
            &events_tx,
            &counter,
            &thread_id,
            &cwd_str,
        )
        .await?;

        // We need to take the child's reference out of the Handshake
        // bundle into the AgentSession we return below, but the pump
        // also needs to keep reading from the same `reader` and stdin.
        // Move reader + stdin into the spawned task; keep `child`
        // owned by the AgentSession.
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
                |session_id, msg| make_error_event(session_id, msg),
                &mut stdin_holder,
            )
            .await;
        });

        // The Handshake bundle owns `child`, but it was destructured
        // above into the local `child`. Hand it to AgentSession.
        // (The `mut` on child above is so we could take stdin/stdout/
        // stderr at handshake time.)
        let _ = &mut child; // borrow-check appeasement (we already moved its handles)

        Ok(AgentSession::new(
            thread_id, child, events_rx, stderr, started_at, counter,
        ))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Persistent session for chat
// ──────────────────────────────────────────────────────────────────────

/// Long-lived `codex app-server` connection shared across many turns.
///
/// Construction (`open`) runs the initialize / thread/start handshake
/// once. After that, `send_turn(prompt)` issues a `turn/start` request
/// and returns a [`TurnHandle`] whose event channel drains until the
/// turn's `turn/completed` notification.
///
/// Cancellation is per-turn via [`CodexInterruptHandle`] (obtained from
/// [`interrupt_handle`](Self::interrupt_handle)): it sends
/// `turn/interrupt`; codex emits `turn/completed { status:
/// "interrupted" }` shortly after, closing the handle's channel
/// naturally — the session itself stays open.
///
/// Closing the session (`close`) writes EOF on stdin so codex shuts
/// down cleanly and the child exits with status 0.
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

struct ActiveTurn {
    /// JSON-RPC request id we used for this turn's `turn/start`.
    /// An error response on this id surfaces as an Error event +
    /// channel close.
    request_id: i64,
    /// Filled in when we observe `turn/started` (carries `turn.id`).
    /// Needed for `turn/interrupt`.
    turn_id: Option<String>,
    events_tx: mpsc::Sender<AgentEvent>,
    projector: AppServerProjector,
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
    /// outside (e.g., a [`TurnEvents`](crate::session::TurnEvents)
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
                approval_policy: APPROVAL_POLICY,
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

// ──────────────────────────────────────────────────────────────────────
// Shared handshake
// ──────────────────────────────────────────────────────────────────────

struct Handshake {
    child: Child,
    stdin: ChildStdin,
    reader: Lines<BufReader<ChildStdout>>,
    stderr: stderr_buf::Handle,
    thread_id: String,
    cwd_str: String,
}

async fn handshake(
    binary_path: &std::path::Path,
    profile: &CodexProfile,
    cwd: &std::path::Path,
    api_key: Option<&str>,
    resume: Option<&str>,
) -> Result<Handshake, SpawnError> {
    if !binary_path.exists() {
        return Err(SpawnError::BinaryNotFound(binary_path.to_path_buf()));
    }
    if !profile.path.is_dir() {
        return Err(SpawnError::ProfilePathInvalid(profile.path.clone()));
    }

    let mut cmd = Command::new(binary_path);
    cmd.env("CODEX_HOME", &profile.path);
    if let Some(key) = api_key {
        cmd.env("OPENAI_API_KEY", key);
    }
    cmd.current_dir(cwd);
    cmd.arg("app-server");
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(SpawnError::ProcessSpawn)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("child had no stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("child had no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("child had no stderr")))?;

    // Stderr drain → rolling buffer for diagnostics.
    let stderr_handle = stderr_buf::Handle::new();
    let stderr_for_task = stderr_handle.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::with_capacity(1024);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => stderr_for_task.append(&buf),
                Err(_) => break,
            }
        }
    });

    let mut reader = BufReader::new(stdout).lines();
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?
        .to_owned();

    // initialize
    write_request(
        &mut stdin,
        INITIALIZE_REQUEST_ID,
        "initialize",
        InitializeParams {
            client_info: ClientInfo {
                name: "anatta",
                title: "anatta",
                version: env!("CARGO_PKG_VERSION"),
            },
        },
    )
    .await?;
    wait_for_response(&mut reader, INITIALIZE_REQUEST_ID, &stderr_handle).await?;

    // initialized notification
    write_notification(&mut stdin, "initialized", InitializedParams {}).await?;

    // thread/start or thread/resume
    let thread_id = match resume {
        None => {
            write_request(
                &mut stdin,
                THREAD_REQUEST_ID,
                "thread/start",
                ThreadStartParams {
                    approval_policy: APPROVAL_POLICY,
                    cwd: &cwd_str,
                    sandbox: SANDBOX_POLICY,
                },
            )
            .await?;
            extract_thread_id(
                wait_for_response(&mut reader, THREAD_REQUEST_ID, &stderr_handle).await?,
            )
            .ok_or_else(|| {
                SpawnError::Io(std::io::Error::other(
                    "thread/start response missing thread.id",
                ))
            })?
        }
        Some(id) => {
            write_request(
                &mut stdin,
                THREAD_REQUEST_ID,
                "thread/resume",
                ThreadResumeParams {
                    thread_id: id,
                    approval_policy: APPROVAL_POLICY,
                    cwd: &cwd_str,
                    sandbox: SANDBOX_POLICY,
                },
            )
            .await?;
            let _ = wait_for_response(&mut reader, THREAD_REQUEST_ID, &stderr_handle).await?;
            id.to_owned()
        }
    };

    Ok(Handshake {
        child,
        stdin,
        reader,
        stderr: stderr_handle,
        thread_id,
        cwd_str,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Reader loops
// ──────────────────────────────────────────────────────────────────────

/// One-shot pump. Drains lines, projects notifications, sends events.
/// On `turn_done_pred` returning true (i.e. turn/completed observed),
/// closes stdin so codex exits cleanly.
async fn run_pump<F, E>(
    reader: &mut Lines<BufReader<ChildStdout>>,
    projector: &mut AppServerProjector,
    events_tx: &mpsc::Sender<AgentEvent>,
    counter: &AtomicU64,
    stderr: &stderr_buf::Handle,
    turn_done_pred: F,
    make_error: E,
    stdin_holder: &mut Option<ChildStdin>,
) where
    F: Fn(&str) -> bool,
    E: Fn(&str, &str) -> AgentEvent,
{
    let session_id = projector_session_id_owned(projector);
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => {
                        stderr.append(line.as_bytes());
                        continue;
                    }
                };
                if value.get("id").is_some() {
                    if let Some(err) = value.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(unknown)");
                        let _ = events_tx.send(make_error(&session_id, msg)).await;
                        drop(stdin_holder.take());
                        return;
                    }
                    continue;
                }
                let method = value
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let params = value
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let turn_done = turn_done_pred(method);
                for ev in projector.project(method, &params) {
                    if events_tx.send(ev).await.is_err() {
                        return;
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if turn_done {
                    drop(stdin_holder.take());
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Persistent reader. Routes notifications to the currently-active
/// turn (if any), routes error responses for the active turn's
/// request id to an Error event + close.
async fn persistent_reader_loop(
    mut reader: Lines<BufReader<ChildStdout>>,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    counter: Arc<AtomicU64>,
    stderr: stderr_buf::Handle,
    thread_id: String,
) {
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => {
                        stderr.append(line.as_bytes());
                        continue;
                    }
                };
                // Response branch
                if let Some(id) = value.get("id").and_then(|v| v.as_i64()) {
                    if let Some(err) = value.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(unknown)")
                            .to_string();
                        let mut at = active.lock().await;
                        let take = matches!(&*at, Some(t) if t.request_id == id);
                        if take {
                            let ActiveTurn {
                                events_tx,
                                ..
                            } = at.take().unwrap();
                            let _ = events_tx.send(make_error_event(&thread_id, &msg)).await;
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        // else: error for a non-turn request (e.g.
                        // turn/interrupt). Surface to stderr buffer.
                        stderr.append(format!("error id={id}: {msg}\n").as_bytes());
                    }
                    continue;
                }
                // Notification branch
                let method = value
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let params = value
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let mut at = active.lock().await;
                let Some(turn) = at.as_mut() else {
                    // No active turn — drop. (mcpServer startup,
                    // thread/status/changed between turns, etc.)
                    continue;
                };

                // Capture turn_id when we see turn/started so future
                // interrupts know which turn to target.
                if method == "turn/started" {
                    turn.turn_id = params
                        .get("turn")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }

                let turn_done = method == "turn/completed";
                for ev in turn.projector.project(method, &params) {
                    if turn.events_tx.send(ev).await.is_err() {
                        // Receiver dropped — turn aborted on consumer
                        // side. Clear and continue.
                        *at = None;
                        break;
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if turn_done {
                    // Drop the sender (channel closes on receiver side).
                    *at = None;
                }
            }
            Ok(None) => {
                // EOF — server exited. Tear down any active turn.
                let mut at = active.lock().await;
                *at = None;
                break;
            }
            Err(_) => break,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// JSON-RPC I/O helpers
// ──────────────────────────────────────────────────────────────────────

async fn write_request<P: serde::Serialize>(
    stdin: &mut ChildStdin,
    id: i64,
    method: &str,
    params: P,
) -> Result<(), SpawnError> {
    let req = OutgoingRequest::new(id, method, params);
    let mut s = serde_json::to_string(&req)
        .map_err(|e| SpawnError::Io(std::io::Error::other(format!("serialize request: {e}"))))?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await.map_err(SpawnError::Io)?;
    stdin.flush().await.map_err(SpawnError::Io)?;
    Ok(())
}

async fn write_notification<P: serde::Serialize>(
    stdin: &mut ChildStdin,
    method: &str,
    params: P,
) -> Result<(), SpawnError> {
    let n = OutgoingNotification::new(method, params);
    let mut s = serde_json::to_string(&n).map_err(|e| {
        SpawnError::Io(std::io::Error::other(format!("serialize notification: {e}")))
    })?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await.map_err(SpawnError::Io)?;
    stdin.flush().await.map_err(SpawnError::Io)?;
    Ok(())
}

async fn wait_for_response(
    reader: &mut Lines<BufReader<ChildStdout>>,
    expected_id: i64,
    stderr: &stderr_buf::Handle,
) -> Result<serde_json::Value, SpawnError> {
    loop {
        let line = match reader.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                return Err(SpawnError::ChildExitedEarly {
                    status: None,
                    stderr_tail: stderr.snapshot(),
                });
            }
            Err(e) => return Err(SpawnError::Io(e)),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                stderr.append(line.as_bytes());
                continue;
            }
        };
        if value.get("id").and_then(|v| v.as_i64()) != Some(expected_id) {
            continue;
        }
        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            return Err(SpawnError::Io(std::io::Error::other(format!(
                "JSON-RPC error on id={expected_id}: {msg}"
            ))));
        }
        return Ok(value
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null));
    }
}

fn extract_thread_id(result: serde_json::Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

async fn push_synthetic_session_started(
    events_tx: &mpsc::Sender<AgentEvent>,
    counter: &AtomicU64,
    thread_id: &str,
    cwd_str: &str,
) -> Result<(), SpawnError> {
    let ev = AgentEvent {
        envelope: anatta_core::AgentEventEnvelope {
            session_id: thread_id.to_owned(),
            timestamp: chrono::Utc::now(),
            backend: anatta_core::Backend::Codex,
            raw_uuid: None,
            parent_tool_use_id: None,
        },
        payload: anatta_core::AgentEventPayload::SessionStarted {
            cwd: cwd_str.to_owned(),
            model: String::new(),
            tools_available: Vec::new(),
        },
    };
    events_tx
        .send(ev)
        .await
        .map_err(|_| SpawnError::Io(std::io::Error::other("consumer channel closed")))?;
    counter.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn make_error_event(thread_id: &str, message: &str) -> AgentEvent {
    AgentEvent {
        envelope: anatta_core::AgentEventEnvelope {
            session_id: thread_id.to_owned(),
            timestamp: chrono::Utc::now(),
            backend: anatta_core::Backend::Codex,
            raw_uuid: None,
            parent_tool_use_id: None,
        },
        payload: anatta_core::AgentEventPayload::Error {
            message: format!("codex JSON-RPC error: {message}"),
            fatal: true,
        },
    }
}

// AppServerProjector is opaque to this module; expose its session_id
// via a tiny helper since we created it.
fn projector_session_id_owned(p: &AppServerProjector) -> String {
    p.session_id().to_owned()
}
