//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anatta_core::{
    AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend, ProjectionContext, Projector,
};
use chrono::Utc;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::{Mutex, mpsc};

use crate::claude::HistoryProjector;
use crate::claude::history::ClaudeEvent;
use crate::conversation::paths::{encode_cwd, working_jsonl_path};
use crate::profile::ClaudeProfile;
use crate::spawn::stderr_buf;
use crate::spawn::{ClaudeSessionId, ExitInfo, SpawnError};

const PTY_ROWS: u16 = 50;
const PTY_COLS: u16 = 200;
/// After spawn/discovery, sleep this long before returning from `open()` so
/// the PTY input handler has time to be ready for bracketed-paste keystrokes
/// from the first `send_turn`.
const STARTUP_SLEEP: Duration = Duration::from_millis(500);
const SESSION_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_DISCOVERY_INTERVAL: Duration = Duration::from_millis(200);
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

/// Number of Shift+Tab keystrokes required to advance claude's
/// internal permission-mode cursor from `from` to `to`, given that
/// each Shift+Tab moves one slot forward in `PermissionLevel::CYCLE`.
pub(crate) fn shift_tab_count(
    from: anatta_core::PermissionLevel,
    to: anatta_core::PermissionLevel,
) -> usize {
    let cycle = anatta_core::PermissionLevel::CYCLE;
    let f = cycle.iter().position(|&l| l == from).unwrap_or(0);
    let t = cycle.iter().position(|&l| l == to).unwrap_or(0);
    (t + cycle.len() - f) % cycle.len()
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
async fn run_tail(path: PathBuf, events_tx: mpsc::Sender<AgentEvent>, session_id: String) {
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
        if reader
            .seek(std::io::SeekFrom::Start(byte_offset))
            .await
            .is_err()
        {
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
    /// `Some(id)` → `--resume <id>`. `None` → fresh session; claude chooses
    /// the UUID and we discover it from the newly-created session JSONL.
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
    /// Initial permission level. Mapped to `--permission-mode <value>`
    /// at spawn; the session tracks subsequent transitions via
    /// `set_permission_level`.
    pub permission_level: anatta_core::PermissionLevel,
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

struct FreshSessionDiscovery {
    profile_dir: PathBuf,
    cwd: String,
    project_jsonl_dir: PathBuf,
    pre_spawn_jsonls: HashSet<String>,
}

// ──────────────────────────────────────────────────────────────────────
// Session
// ──────────────────────────────────────────────────────────────────────

/// Long-lived interactive `claude` connection.
pub struct ClaudeInteractiveSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[allow(dead_code)] // held so resize(...) can be added later
    master: Box<dyn MasterPty + Send>,
    session_id: OnceLock<ClaudeSessionId>,
    fresh_discovery: Option<FreshSessionDiscovery>,
    pty_tx: mpsc::Sender<PtyCommand>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    stderr: stderr_buf::Handle,
    started_at: Instant,
    events_emitted: Arc<AtomicU64>,
    /// Last level we instructed claude to be at. Used to compute the
    /// number of `\x1b[Z` (Shift+Tab) writes needed to reach a new
    /// target without scraping the TUI's status bar.
    current_level: std::sync::Mutex<anatta_core::PermissionLevel>,
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

        ensure_onboarding_complete(&launch.profile.path).await?;

        let cwd_str = launch
            .cwd
            .to_str()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;
        let project_jsonl_dir = project_jsonl_dir(&launch.profile.path, cwd_str);
        let pre_spawn_jsonls = if launch.resume.is_none() {
            Some(snapshot_jsonl_stems(&project_jsonl_dir).await?)
        } else {
            None
        };

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
        cmd.arg("--permission-mode");
        cmd.arg(launch.permission_level.claude_arg());
        if launch.bare {
            cmd.arg("--bare");
        }
        if let Some(m) = &launch.model {
            cmd.arg("--model");
            cmd.arg(m);
        }
        if let Some(resume) = &launch.resume {
            cmd.arg("--resume");
            cmd.arg(resume.as_str());
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

        let active_turn: Arc<Mutex<Option<ActiveTurn>>> = Arc::new(Mutex::new(None));
        let events_emitted = Arc::new(AtomicU64::new(0));
        let session_id = OnceLock::new();
        let fresh_discovery = if let Some(resume) = launch.resume.clone() {
            let _ = session_id.set(resume.clone());
            let jsonl = working_jsonl_path(&launch.profile.path, cwd_str, resume.as_str());
            let initial_tail_offset = tokio::fs::metadata(&jsonl)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            spawn_persistent_tail(
                jsonl,
                active_turn.clone(),
                events_emitted.clone(),
                resume.as_str().to_owned(),
                initial_tail_offset,
            );
            None
        } else {
            Some(FreshSessionDiscovery {
                profile_dir: launch.profile.path.clone(),
                cwd: cwd_str.to_owned(),
                project_jsonl_dir,
                pre_spawn_jsonls: pre_spawn_jsonls.unwrap_or_default(),
            })
        };

        // Give claude's PTY input handler a moment to attach before the first
        // bracketed-paste prompt. Fresh session-id discovery starts after that
        // first prompt because claude 2.1.x creates the JSONL lazily.
        tokio::time::sleep(STARTUP_SLEEP).await;

        let stderr_handle = stderr_buf::Handle::new();

        Ok(Self {
            child,
            master: pair.master,
            session_id,
            fresh_discovery,
            pty_tx,
            active_turn,
            stderr: stderr_handle,
            started_at: Instant::now(),
            events_emitted,
            current_level: std::sync::Mutex::new(launch.permission_level),
        })
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.get().map(|id| id.as_str())
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

    /// Cycle claude's TUI permission mode by writing `\x1b[Z` (Shift+Tab)
    /// `N` times via the PTY writer, where `N` is the forward distance
    /// from the current level to `target` in `PermissionLevel::CYCLE`.
    /// Updates the local tracker so subsequent calls compute correctly.
    pub async fn set_permission_level(
        &self,
        target: anatta_core::PermissionLevel,
    ) -> Result<(), SpawnError> {
        let n = {
            let mut guard = self
                .current_level
                .lock()
                .expect("permission_level mutex poisoned");
            let n = shift_tab_count(*guard, target);
            *guard = target;
            n
        };
        if n == 0 {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(n * 3);
        for _ in 0..n {
            bytes.extend_from_slice(b"\x1b[Z");
        }
        self.pty_tx
            .send(PtyCommand::Write(bytes))
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("pty writer task gone")))
    }

    /// Current tracked permission level.
    pub fn permission_level(&self) -> anatta_core::PermissionLevel {
        *self
            .current_level
            .lock()
            .expect("permission_level mutex poisoned")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Persistent tail
// ──────────────────────────────────────────────────────────────────────

fn spawn_persistent_tail(
    path: PathBuf,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    counter: Arc<AtomicU64>,
    session_id: String,
    initial_byte_offset: u64,
) {
    tokio::spawn(async move {
        persistent_tail_loop(path, active, counter, session_id, initial_byte_offset).await;
    });
}

/// Like `run_tail`, but routes events to the currently-active turn (if
/// any) instead of a single fixed channel. Closes the active turn on
/// `AgentEventPayload::TurnCompleted`.
async fn persistent_tail_loop(
    path: PathBuf,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    counter: Arc<AtomicU64>,
    session_id: String,
    initial_byte_offset: u64,
) {
    let mut projector = HistoryProjector::new();
    let mut byte_offset: u64 = initial_byte_offset;
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
        if reader
            .seek(std::io::SeekFrom::Start(byte_offset))
            .await
            .is_err()
        {
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

/// Pre-seed `<profile>/.claude.json` so claude's interactive TUI skips
/// first-run onboarding (theme picker, etc.) that would otherwise swallow
/// our first bracketed-paste prompt. Idempotent: only writes the keys
/// claude needs to bypass the wizard; existing keys (credentials,
/// theme already chosen by the user, etc.) are preserved.
///
/// Pre-seed every `<CLAUDE_CONFIG_DIR>/.claude.json` marker we know
/// suppresses a first-run TUI screen that would otherwise eat the
/// first bracketed-paste prompt:
///
/// - `hasCompletedOnboarding` — skips the theme picker.
/// - `theme` — fallback in case the picker still tries to run.
/// - `lastReleaseNotesSeen` — claude shows a "What's new in {version}"
///   panel at startup whenever this is below the running CLI version
///   (or absent). The panel sits next to a welcome panel and they
///   both capture keystrokes until dismissed. Setting a far-future
///   sentinel makes the panel skip forever.
/// - `autoPermissionsNotificationCount` + `hasResetAutoModeOptInForDefaultOffer`
///   — skips the "Enable auto mode?" opt-in dialog that fires the
///   first time `--permission-mode auto` is used in this profile.
///
/// Each marker is added independently if missing — the function runs
/// every `open()` and is safe to re-run on profiles that already have
/// some but not all markers (e.g. profiles seeded by an older anatta).
async fn ensure_onboarding_complete(profile_dir: &Path) -> Result<(), SpawnError> {
    let claude_json = profile_dir.join(".claude.json");
    let mut config: serde_json::Value = match tokio::fs::read_to_string(&claude_json).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(e) if e.kind() == ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(SpawnError::Io(e)),
    };
    let obj = match config.as_object_mut() {
        Some(o) => o,
        None => return Ok(()), // .claude.json exists but isn't an object; leave alone
    };

    let mut changed = false;

    if !obj.contains_key("hasCompletedOnboarding") {
        obj.insert(
            "hasCompletedOnboarding".to_string(),
            serde_json::Value::Bool(true),
        );
        changed = true;
    }
    if !obj.contains_key("theme") {
        obj.insert(
            "theme".to_string(),
            serde_json::Value::String("dark".to_string()),
        );
        changed = true;
    }
    if !obj.contains_key("lastReleaseNotesSeen") {
        // Sentinel that's always >= claude's current version, so the
        // "What's new in X" panel never fires for anatta-managed
        // sessions regardless of which claude version is installed.
        obj.insert(
            "lastReleaseNotesSeen".to_string(),
            serde_json::Value::String("999.999.999".to_string()),
        );
        changed = true;
    }
    if !obj.contains_key("autoPermissionsNotificationCount") {
        obj.insert(
            "autoPermissionsNotificationCount".to_string(),
            serde_json::Value::Number(1.into()),
        );
        changed = true;
    }
    if !obj.contains_key("hasResetAutoModeOptInForDefaultOffer") {
        obj.insert(
            "hasResetAutoModeOptInForDefaultOffer".to_string(),
            serde_json::Value::Bool(true),
        );
        changed = true;
    }

    if !changed {
        return Ok(());
    }

    let pretty = serde_json::to_string_pretty(&config).map_err(|e| {
        SpawnError::Io(std::io::Error::other(format!("serialize claude.json: {e}")))
    })?;
    tokio::fs::write(&claude_json, pretty)
        .await
        .map_err(SpawnError::Io)
}

fn project_jsonl_dir(profile_dir: &Path, canonical_cwd: &str) -> PathBuf {
    profile_dir.join("projects").join(encode_cwd(canonical_cwd))
}

async fn snapshot_jsonl_stems(dir: &Path) -> Result<HashSet<String>, SpawnError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(SpawnError::Io(e)),
    };
    let mut stems = HashSet::new();
    while let Some(entry) = entries.next_entry().await.map_err(SpawnError::Io)? {
        if !entry.file_type().await.map_err(SpawnError::Io)?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        stems.insert(stem.to_owned());
    }
    Ok(stems)
}

async fn discover_new_session_id(
    dir: &Path,
    pre_spawn_jsonls: &HashSet<String>,
) -> Result<ClaudeSessionId, SpawnError> {
    let started = Instant::now();
    loop {
        let current = snapshot_jsonl_stems(dir).await?;
        let mut new_stems: Vec<String> = current
            .difference(pre_spawn_jsonls)
            .map(ToOwned::to_owned)
            .collect();
        if !new_stems.is_empty() {
            new_stems.sort();
            return Ok(ClaudeSessionId::new(new_stems.remove(0)));
        }
        if started.elapsed() >= SESSION_DISCOVERY_TIMEOUT {
            return Err(SpawnError::Io(std::io::Error::new(
                ErrorKind::TimedOut,
                format!(
                    "claude did not create a new session JSONL in {} within {:?}",
                    dir.display(),
                    SESSION_DISCOVERY_TIMEOUT
                ),
            )));
        }
        tokio::time::sleep(SESSION_DISCOVERY_INTERVAL).await;
    }
}

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
    /// Shut the session down by sending `/exit` to claude's TUI (the
    /// only signal that makes claude code run its graceful save-and-exit
    /// path within a few seconds — SIGHUP via dropped-master does not).
    /// Wait up to `CLOSE_GRACE` for natural exit; otherwise SIGKILL.
    /// portable-pty's `Child::wait` is sync, so we drive it on a
    /// blocking task.
    ///
    /// `/exit` causes claude code to synthesize a small `<local-command-*>`
    /// transcript block + a `model: "<synthetic>"` "No response requested."
    /// assistant turn into the session JSONL. These pollute the on-disk
    /// record but are filtered out by `HistoryProjector` before reaching
    /// any rendered surface (CLI output, central → render pipeline).
    pub async fn close(self) -> Result<ExitInfo, SpawnError> {
        let started_at = self.started_at;
        let stderr = self.stderr;
        let events_emitted = self.events_emitted;
        let pty_tx = self.pty_tx.clone();
        let mut child = self.child;

        // Clone a killer *before* moving `child` into the blocking task so we
        // can send SIGKILL on the timeout path without needing the child handle.
        let mut killer = child.clone_killer();

        // Ask claude to /exit gracefully. SIGHUP alone (via dropping
        // the PTY master) does not make claude code exit within the
        // grace window; the slash command does. We accept the small
        // disk-pollution cost — the projector filters the resulting
        // synthetic events from every rendered output.
        let _ = pty_tx.send(PtyCommand::Write(b"/exit\r".to_vec())).await;

        let wait_handle = tokio::task::spawn_blocking(move || child.wait());

        match tokio::time::timeout(CLOSE_GRACE, wait_handle).await {
            Ok(joined) => {
                let status = joined
                    .map_err(|e| SpawnError::Io(std::io::Error::other(e)))?
                    .map_err(|e| {
                        SpawnError::Io(std::io::Error::other(format!("child.wait: {e}")))
                    })?;
                Ok(ExitInfo {
                    exit_code: Some(status.exit_code() as i32),
                    signal: None,
                    duration: started_at.elapsed(),
                    stderr_tail: stderr.snapshot(),
                    events_emitted: events_emitted.load(Ordering::Relaxed),
                })
            }
            Err(_) => {
                // Grace period expired — drop master + SIGKILL the child
                // so it doesn't linger.
                let _ = pty_tx.send(PtyCommand::Shutdown).await;
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
        let prompt_bytes = encode_prompt(prompt);

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

        if let Some(session_id) = self.session_id() {
            if let Err(e) =
                push_synthetic_session_started(&events_tx, &self.events_emitted, session_id, "")
                    .await
            {
                *self.active_turn.lock().await = None;
                return Err(e);
            }

            if self
                .pty_tx
                .send(PtyCommand::Write(prompt_bytes))
                .await
                .is_err()
            {
                *self.active_turn.lock().await = None;
                return Err(SpawnError::Io(std::io::Error::other(
                    "pty writer task gone",
                )));
            }
            return Ok(InteractiveTurnHandle { events_rx });
        }

        if self
            .pty_tx
            .send(PtyCommand::Write(prompt_bytes))
            .await
            .is_err()
        {
            *self.active_turn.lock().await = None;
            return Err(SpawnError::Io(std::io::Error::other(
                "pty writer task gone",
            )));
        }

        let discovery = self.fresh_discovery.as_ref().ok_or_else(|| {
            SpawnError::Io(std::io::Error::other(
                "fresh claude session has no discovery state",
            ))
        })?;
        let session_id = match discover_new_session_id(
            &discovery.project_jsonl_dir,
            &discovery.pre_spawn_jsonls,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                *self.active_turn.lock().await = None;
                return Err(e);
            }
        };
        let _ = self.session_id.set(session_id.clone());

        if let Err(e) = push_synthetic_session_started(
            &events_tx,
            &self.events_emitted,
            session_id.as_str(),
            "",
        )
        .await
        {
            *self.active_turn.lock().await = None;
            return Err(e);
        }

        let jsonl = working_jsonl_path(&discovery.profile_dir, &discovery.cwd, session_id.as_str());
        spawn_persistent_tail(
            jsonl,
            self.active_turn.clone(),
            self.events_emitted.clone(),
            session_id.as_str().to_owned(),
            0,
        );

        Ok(InteractiveTurnHandle { events_rx })
    }
}

#[cfg(test)]
mod tests_perm {
    use super::shift_tab_count;
    use anatta_core::PermissionLevel;

    #[test]
    fn shift_tab_count_zero_when_same() {
        assert_eq!(
            shift_tab_count(PermissionLevel::Default, PermissionLevel::Default),
            0
        );
    }

    #[test]
    fn shift_tab_count_steps_forward_in_cycle() {
        // CYCLE = [Default, AcceptEdits, Auto, BypassAll, Plan]
        assert_eq!(
            shift_tab_count(PermissionLevel::Default, PermissionLevel::AcceptEdits),
            1
        );
        assert_eq!(
            shift_tab_count(PermissionLevel::Default, PermissionLevel::Auto),
            2
        );
        assert_eq!(
            shift_tab_count(PermissionLevel::Default, PermissionLevel::BypassAll),
            3
        );
        assert_eq!(
            shift_tab_count(PermissionLevel::Default, PermissionLevel::Plan),
            4
        );
    }

    #[test]
    fn shift_tab_count_wraps_backwards_via_forward_steps() {
        // Plan → Default is 1 forward step (wraps).
        assert_eq!(
            shift_tab_count(PermissionLevel::Plan, PermissionLevel::Default),
            1
        );
        // BypassAll → AcceptEdits = forward through Plan, Default, AcceptEdits = 3.
        assert_eq!(
            shift_tab_count(PermissionLevel::BypassAll, PermissionLevel::AcceptEdits),
            3
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_jsonl_stems_only_records_jsonl_stems() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("old-session.jsonl"), "").unwrap();
        std::fs::write(tmp.path().join("not-json.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sidecar.jsonl")).unwrap();

        let stems = snapshot_jsonl_stems(tmp.path()).await.unwrap();

        assert!(stems.contains("old-session"));
        assert!(!stems.contains("not-json"));
        assert!(!stems.contains("sidecar"));
    }

    #[tokio::test]
    async fn discover_new_session_id_returns_new_jsonl_stem() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("existing.jsonl"), "").unwrap();
        let before = snapshot_jsonl_stems(tmp.path()).await.unwrap();

        let dir = tmp.path().to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(dir.join("actual-session.jsonl"), "").unwrap();
        });

        let id = discover_new_session_id(tmp.path(), &before).await.unwrap();

        assert_eq!(id.as_str(), "actual-session");
    }
}
