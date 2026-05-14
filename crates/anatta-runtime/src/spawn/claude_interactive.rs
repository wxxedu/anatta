//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anatta_core::{AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend, ProjectionContext, Projector};
use chrono::Utc;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use crate::claude::history::ClaudeEvent;
use crate::claude::HistoryProjector;
use crate::conversation::paths::working_jsonl_path;
use crate::profile::ClaudeProfile;
use crate::spawn::stderr_buf;
use crate::spawn::{ClaudeSessionId, ExitInfo, SpawnError};

const PTY_ROWS: u16 = 50;
const PTY_COLS: u16 = 200;
/// After spawn, sleep this long before returning from `open()` so the
/// PTY input handler has time to be ready for bracketed-paste keystrokes
/// from the first `send_turn`. Empirically ~200 ms is enough for claude
/// 2.1.x on macOS; 500 ms is a defensive default.
const STARTUP_SLEEP: Duration = Duration::from_millis(500);
const CLOSE_GRACE: Duration = Duration::from_secs(3);

// ──────────────────────────────────────────────────────────────────────
// Prompt encoding
// ──────────────────────────────────────────────────────────────────────

pub(crate) fn encode_prompt(prompt: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prompt.len() + 13);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(prompt.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

#[doc(hidden)]
pub fn encode_prompt_for_test(prompt: &str) -> Vec<u8> {
    encode_prompt(prompt)
}

// ──────────────────────────────────────────────────────────────────────
// JSONL tail
// ──────────────────────────────────────────────────────────────────────

/// Tail a claude session JSONL: read appended lines, parse as
/// `ClaudeEvent`, project to `AgentEvent`, push into `events_tx`. Close
/// the channel (return) when an `AgentEventPayload::TurnCompleted` is
/// observed OR when `events_tx` is dropped by the consumer.
///
/// Polling interval: 25 ms — cheap (the file is local and kernel-cached)
/// and fast enough that turn boundaries feel immediate. We re-open the
/// file each tick rather than holding it open while idle.
async fn run_tail(
    path: PathBuf,
    events_tx: mpsc::Sender<AgentEvent>,
    session_id: String,
) {
    let mut projector = HistoryProjector::new();
    let mut byte_offset: u64 = 0;
    let mut line_buf = String::new();
    let interval = Duration::from_millis(25);

    loop {
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => {
                tokio::time::sleep(interval).await;
                continue;
            }
        };
        let mut reader = BufReader::new(file);
        if reader.seek(std::io::SeekFrom::Start(byte_offset)).await.is_err() {
            tokio::time::sleep(interval).await;
            continue;
        }

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break, // EOF — fall through to sleep + reopen
                Ok(n) => {
                    // A line without trailing '\n' is partial — back up
                    // and re-read it next tick when the rest arrives.
                    if !line_buf.ends_with('\n') {
                        break;
                    }
                    byte_offset += n as u64;
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let raw: ClaudeEvent = match serde_json::from_str(trimmed) {
                        Ok(r) => r,
                        Err(_) => continue, // skip malformed; parser fixtures own coverage
                    };
                    let ctx = ProjectionContext {
                        session_id: session_id.clone(),
                        received_at: Utc::now(),
                    };
                    for ev in projector.project(&raw, &ctx) {
                        let is_completion =
                            matches!(ev.payload, AgentEventPayload::TurnCompleted { .. });
                        if events_tx.send(ev).await.is_err() {
                            return; // consumer dropped
                        }
                        if is_completion {
                            return; // turn done — close the channel
                        }
                    }
                }
                Err(_) => break,
            }
        }

        tokio::time::sleep(interval).await;
    }
}

#[doc(hidden)]
pub async fn run_tail_for_test(
    path: PathBuf,
    events_tx: mpsc::Sender<AgentEvent>,
    session_id: String,
) {
    run_tail(path, events_tx, session_id).await
}

// ──────────────────────────────────────────────────────────────────────
// Public configuration
// ──────────────────────────────────────────────────────────────────────

/// Configuration for spawning an interactive (PTY-wrapped) claude session.
#[derive(Debug, Clone)]
pub struct ClaudeInteractiveLaunch {
    pub profile: ClaudeProfile,
    /// MUST be canonicalized (e.g. `/tmp` → `/private/tmp` on macOS)
    /// because we derive the JSONL path from it via `working_jsonl_path`
    /// and claude does the same canonicalization internally.
    pub cwd: PathBuf,
    /// `Some(id)` → `--resume <id>`. `None` → fresh session; a new UUID
    /// will be chosen at `open()` and passed via `--session-id`.
    pub resume: Option<ClaudeSessionId>,
    pub binary_path: PathBuf,
    pub provider: Option<crate::profile::ProviderEnv>,
    /// Optional model override (passed as `--model <name>`).
    pub model: Option<String>,
    /// If true, pass `--bare` for a minimal child environment
    /// (no hooks / LSP / plugin sync / CLAUDE.md auto-discovery /
    /// keychain reads). The CLI's `build_claude_interactive` derives this
    /// from `auth_method` — `true` for ApiKey profiles, `false` for Login/
    /// OAuth profiles (which need keychain access that `--bare` disables).
    pub bare: bool,
}

// ──────────────────────────────────────────────────────────────────────
// Internal types
// ──────────────────────────────────────────────────────────────────────

pub(crate) enum PtyCommand {
    Write(Vec<u8>),
    Shutdown,
}

pub(crate) struct ActiveTurn {
    events_tx: mpsc::Sender<AgentEvent>,
}

// ──────────────────────────────────────────────────────────────────────
// Session
// ──────────────────────────────────────────────────────────────────────

/// Long-lived interactive `claude` connection.
pub struct ClaudeInteractiveSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[allow(dead_code)] // held so resize(...) can be added later
    master: Box<dyn MasterPty + Send>,
    session_id: ClaudeSessionId,
    pty_tx: mpsc::Sender<PtyCommand>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    stderr: stderr_buf::Handle,
    started_at: Instant,
    events_emitted: Arc<AtomicU64>,
}

/// Cloneable handle for interrupting the active turn from outside — see
/// [`ClaudeInteractiveSession::interrupt_handle`].
#[derive(Clone)]
pub struct ClaudeInteractiveInterruptHandle {
    pub(crate) pty_tx: mpsc::Sender<PtyCommand>,
    pub(crate) active_turn: Arc<Mutex<Option<ActiveTurn>>>,
}

impl ClaudeInteractiveInterruptHandle {
    /// Cancel the currently-active turn by sending Ctrl-C through the
    /// PTY. Idempotent — no-op if no turn is active or the session is
    /// closed. claude's TUI handles `\x03` as "interrupt current turn";
    /// the JSONL emits a `turn_duration` shortly after, which the tail
    /// task turns into `TurnCompleted` and the channel closes naturally.
    pub async fn interrupt(&self) -> Result<(), SpawnError> {
        {
            let active = self.active_turn.lock().await;
            if active.is_none() {
                return Ok(());
            }
        }
        self.pty_tx
            .send(PtyCommand::Write(vec![0x03]))
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("pty writer task gone")))
    }
}

impl ClaudeInteractiveSession {
    pub async fn open(launch: ClaudeInteractiveLaunch) -> Result<Self, SpawnError> {
        if !launch.binary_path.exists() {
            return Err(SpawnError::BinaryNotFound(launch.binary_path.clone()));
        }
        if !launch.profile.path.is_dir() {
            return Err(SpawnError::ProfilePathInvalid(launch.profile.path.clone()));
        }

        // Pick session id up front so we know the JSONL path without
        // having to discover the filename later.
        let session_id = launch
            .resume
            .clone()
            .unwrap_or_else(|| ClaudeSessionId::new(uuid::Uuid::new_v4().to_string()));

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("openpty: {e}"))))?;

        let mut cmd = CommandBuilder::new(&launch.binary_path);
        cmd.env(
            "CLAUDE_CONFIG_DIR",
            launch.profile.path.to_string_lossy().as_ref(),
        );
        cmd.env(
            "TERM",
            std::env::var("TERM").as_deref().unwrap_or("xterm-256color"),
        );
        if let Some(env) = &launch.provider {
            for (k, v) in &env.vars {
                cmd.env(k, v);
            }
        }
        cmd.cwd(launch.cwd.as_os_str());
        cmd.arg("--session-id");
        cmd.arg(session_id.as_str());
        cmd.arg("--permission-mode");
        cmd.arg("bypassPermissions");
        if launch.bare {
            cmd.arg("--bare");
        }
        if let Some(m) = &launch.model {
            cmd.arg("--model");
            cmd.arg(m);
        }
        if launch.resume.is_some() {
            cmd.arg("--resume");
            cmd.arg(session_id.as_str());
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("spawn: {e}"))))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("clone_reader: {e}"))))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("take_writer: {e}"))))?;

        // Drain thread: must read continuously or the kernel PTY buffer
        // fills, claude blocks on its next write, and the JSONL stops
        // being appended.
        std::thread::Builder::new()
            .name("claude-pty-drain".into())
            .spawn(move || {
                let _ = std::io::copy(&mut reader, &mut std::io::sink());
            })
            .map_err(SpawnError::Io)?;

        // Writer thread: owns the sync `Write`, pulls commands off a
        // tokio mpsc. Dedicated OS thread because portable-pty's writer
        // is blocking.
        let (pty_tx, mut pty_rx) = mpsc::channel::<PtyCommand>(8);
        std::thread::Builder::new()
            .name("claude-pty-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Some(cmd) = pty_rx.blocking_recv() {
                    match cmd {
                        PtyCommand::Write(bytes) => {
                            if std::io::Write::write_all(&mut writer, &bytes).is_err() {
                                break;
                            }
                            let _ = std::io::Write::flush(&mut writer);
                        }
                        PtyCommand::Shutdown => break,
                    }
                }
                // Dropping `writer` closes the master end of the PTY,
                // which sends EOF / SIGHUP to the child.
            })
            .map_err(SpawnError::Io)?;

        let cwd_str = launch
            .cwd
            .to_str()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;
        let jsonl = working_jsonl_path(&launch.profile.path, cwd_str, session_id.as_str());

        // Give claude's PTY input handler a moment to be ready for our first
        // bracketed-paste prompt. Claude defers writing the session JSONL until
        // the first user message, so we cannot use file presence as a readiness
        // signal — the tail task handles "file doesn't exist yet" via retry.
        tokio::time::sleep(STARTUP_SLEEP).await;

        let active_turn: Arc<Mutex<Option<ActiveTurn>>> = Arc::new(Mutex::new(None));
        let events_emitted = Arc::new(AtomicU64::new(0));
        let stderr_handle = stderr_buf::Handle::new();

        // Tail task: dispatches projected events to the currently-
        // active turn (if any).
        let active_for_tail = active_turn.clone();
        let counter_for_tail = events_emitted.clone();
        let session_id_for_tail = session_id.as_str().to_owned();
        tokio::spawn(async move {
            persistent_tail_loop(
                jsonl,
                active_for_tail,
                counter_for_tail,
                session_id_for_tail,
            )
            .await;
        });

        Ok(Self {
            child,
            master: pair.master,
            session_id,
            pty_tx,
            active_turn,
            stderr: stderr_handle,
            started_at: Instant::now(),
            events_emitted,
        })
    }

    pub fn session_id(&self) -> &str {
        self.session_id.as_str()
    }

    pub async fn is_idle(&self) -> bool {
        self.active_turn.lock().await.is_none()
    }

    pub fn interrupt_handle(&self) -> ClaudeInteractiveInterruptHandle {
        ClaudeInteractiveInterruptHandle {
            pty_tx: self.pty_tx.clone(),
            active_turn: self.active_turn.clone(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Persistent tail
// ──────────────────────────────────────────────────────────────────────

/// Like `run_tail`, but routes events to the currently-active turn (if
/// any) instead of a single fixed channel. Closes the active turn on
/// `AgentEventPayload::TurnCompleted`.
async fn persistent_tail_loop(
    path: PathBuf,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    counter: Arc<AtomicU64>,
    session_id: String,
) {
    let mut projector = HistoryProjector::new();
    let mut byte_offset: u64 = 0;
    let mut line_buf = String::new();
    let interval = Duration::from_millis(25);

    loop {
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => {
                tokio::time::sleep(interval).await;
                continue;
            }
        };
        let mut reader = BufReader::new(file);
        if reader.seek(std::io::SeekFrom::Start(byte_offset)).await.is_err() {
            tokio::time::sleep(interval).await;
            continue;
        }

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if !line_buf.ends_with('\n') {
                        break;
                    }
                    byte_offset += n as u64;
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let raw: ClaudeEvent = match serde_json::from_str(trimmed) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let ctx = ProjectionContext {
                        session_id: session_id.clone(),
                        received_at: Utc::now(),
                    };
                    let evs = projector.project(&raw, &ctx);

                    let mut at = active.lock().await;
                    let Some(turn) = at.as_mut() else {
                        // No active turn — nothing to dispatch to. We
                        // still advanced `byte_offset` so we don't
                        // re-read these lines next tick.
                        continue;
                    };
                    let mut completed = false;
                    for ev in evs {
                        completed |= matches!(ev.payload, AgentEventPayload::TurnCompleted { .. });
                        if turn.events_tx.send(ev).await.is_err() {
                            *at = None;
                            break;
                        }
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    if completed {
                        *at = None;
                    }
                }
                Err(_) => break,
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

async fn push_synthetic_session_started(
    events_tx: &mpsc::Sender<AgentEvent>,
    counter: &AtomicU64,
    session_id: &str,
    cwd_str: &str,
) -> Result<(), SpawnError> {
    let ev = AgentEvent {
        envelope: AgentEventEnvelope {
            session_id: session_id.to_owned(),
            timestamp: Utc::now(),
            backend: Backend::Claude,
            raw_uuid: None,
            parent_tool_use_id: None,
        },
        payload: AgentEventPayload::SessionStarted {
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

// ──────────────────────────────────────────────────────────────────────
// Turn handle
// ──────────────────────────────────────────────────────────────────────

/// Per-turn handle returned by [`ClaudeInteractiveSession::send_turn`].
///
/// Drain `events()` until it returns `None`; that signals end-of-turn.
pub struct InteractiveTurnHandle {
    events_rx: mpsc::Receiver<AgentEvent>,
}

impl InteractiveTurnHandle {
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events_rx
    }
}

impl ClaudeInteractiveSession {
    /// Shut the session down: tell the writer thread to drop the master
    /// (which sends SIGHUP to the child), wait up to `CLOSE_GRACE` for
    /// natural exit, otherwise surface a timeout error. portable-pty's
    /// `Child::wait` is sync, so we drive it on a blocking task.
    pub async fn close(self) -> Result<ExitInfo, SpawnError> {
        let _ = self.pty_tx.send(PtyCommand::Shutdown).await;
        let started_at = self.started_at;
        let stderr = self.stderr;
        let events_emitted = self.events_emitted;
        let mut child = self.child;

        // Clone a killer *before* moving `child` into the blocking task so we
        // can send SIGKILL on the timeout path without needing the child handle.
        let mut killer = child.clone_killer();

        let wait_handle = tokio::task::spawn_blocking(move || child.wait());

        match tokio::time::timeout(CLOSE_GRACE, wait_handle).await {
            Ok(joined) => {
                let status = joined
                    .map_err(|e| SpawnError::Io(std::io::Error::other(e)))?
                    .map_err(|e| SpawnError::Io(std::io::Error::other(format!(
                        "child.wait: {e}"
                    ))))?;
                Ok(ExitInfo {
                    exit_code: Some(status.exit_code() as i32),
                    signal: None,
                    duration: started_at.elapsed(),
                    stderr_tail: stderr.snapshot(),
                    events_emitted: events_emitted.load(Ordering::Relaxed),
                })
            }
            Err(_) => {
                // Grace period expired — kill the child so it doesn't linger.
                let _ = killer.kill();
                Err(SpawnError::Io(std::io::Error::other(
                    "interactive claude did not exit within close grace",
                )))
            }
        }
    }
}

impl ClaudeInteractiveSession {
    /// Send a new turn. Refuses if the previous turn is still active.
    pub async fn send_turn(&self, prompt: &str) -> Result<InteractiveTurnHandle, SpawnError> {
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);

        {
            let mut active = self.active_turn.lock().await;
            if active.is_some() {
                return Err(SpawnError::Io(std::io::Error::other(
                    "send_turn called while previous turn still active",
                )));
            }
            *active = Some(ActiveTurn {
                events_tx: events_tx.clone(),
            });
        }

        if let Err(e) = push_synthetic_session_started(
            &events_tx,
            &self.events_emitted,
            self.session_id.as_str(),
            "",
        )
        .await
        {
            *self.active_turn.lock().await = None;
            return Err(e);
        }

        if self
            .pty_tx
            .send(PtyCommand::Write(encode_prompt(prompt)))
            .await
            .is_err()
        {
            *self.active_turn.lock().await = None;
            return Err(SpawnError::Io(std::io::Error::other("pty writer task gone")));
        }

        Ok(InteractiveTurnHandle { events_rx })
    }
}
