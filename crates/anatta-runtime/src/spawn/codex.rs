//! Codex `app-server` (JSON-RPC 2.0 over stdio) launch.
//!
//! Per-turn protocol:
//!
//! ```text
//!   anatta                                 codex app-server
//!     --- initialize ---------------------------> id=0
//!     <--- initialize result --------------------
//!     --- initialized (notification) ----------->
//!     --- thread/start or thread/resume --------> id=1
//!     <--- thread/start result (thread.id) ------
//!     [<--- thread/started notification ---------]
//!     --- turn/start (prompt) ------------------> id=2
//!     <--- turn/start result --------------------
//!     [<--- turn/started notification ----------]
//!     [<--- item/* + agentMessage/delta + ... --]  (streamed)
//!     <--- turn/completed -----------------------
//!     --- (close stdin) ------------------------>  graceful shutdown
//! ```
//!
//! `AgentSession.session_id()` = the thread id captured from the
//! `thread/start` response. Stored as `backend_session_id` on
//! `conversations` so subsequent turns / `anatta send --resume <id>`
//! can use `thread/resume`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anatta_core::AgentEvent;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

use crate::codex::app_server::wire::{
    ClientInfo, InitializeParams, InitializedParams, OutgoingNotification, OutgoingRequest,
    ThreadResumeParams, ThreadStartParams, TurnInput, TurnStartParams,
};
use crate::codex::app_server::AppServerProjector;
use crate::profile::CodexProfile;

use super::stderr_buf;
use super::{AgentSession, CodexThreadId, Launchable, SpawnError};

const APPROVAL_POLICY: &str = "never";
const SANDBOX_POLICY: &str = "danger-full-access";

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
        if !self.binary_path.exists() {
            return Err(SpawnError::BinaryNotFound(self.binary_path.clone()));
        }
        if !self.profile.path.is_dir() {
            return Err(SpawnError::ProfilePathInvalid(self.profile.path.clone()));
        }

        let started_at = Instant::now();

        let mut cmd = Command::new(&self.binary_path);
        cmd.env("CODEX_HOME", &self.profile.path);
        if let Some(key) = &self.api_key {
            cmd.env("OPENAI_API_KEY", key);
        }
        cmd.current_dir(&self.cwd);
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
        let cwd_str = self
            .cwd
            .to_str()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;

        // 1. initialize handshake
        write_request(
            &mut stdin,
            0,
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
        wait_for_response(&mut reader, 0, &stderr_handle).await?;

        // 2. initialized notification
        write_notification(&mut stdin, "initialized", InitializedParams {}).await?;

        // 3. thread/start or thread/resume → capture thread.id
        let thread_id = match self.resume.as_ref() {
            None => {
                write_request(
                    &mut stdin,
                    1,
                    "thread/start",
                    ThreadStartParams {
                        approval_policy: APPROVAL_POLICY,
                        cwd: cwd_str,
                        sandbox: SANDBOX_POLICY,
                    },
                )
                .await?;
                extract_thread_id(wait_for_response(&mut reader, 1, &stderr_handle).await?)
                    .ok_or_else(|| SpawnError::Io(std::io::Error::other(
                        "thread/start response missing thread.id",
                    )))?
            }
            Some(id) => {
                write_request(
                    &mut stdin,
                    1,
                    "thread/resume",
                    ThreadResumeParams {
                        thread_id: id.as_str(),
                        approval_policy: APPROVAL_POLICY,
                        cwd: cwd_str,
                        sandbox: SANDBOX_POLICY,
                    },
                )
                .await?;
                let _ = wait_for_response(&mut reader, 1, &stderr_handle).await?;
                id.as_str().to_owned()
            }
        };

        // 4. turn/start with the user's prompt
        write_request(
            &mut stdin,
            2,
            "turn/start",
            TurnStartParams {
                thread_id: &thread_id,
                input: vec![TurnInput::Text {
                    text: &self.prompt,
                }],
                approval_policy: APPROVAL_POLICY,
                cwd: cwd_str,
            },
        )
        .await?;

        // 5. Spawn the notification pump. Emits AgentEvents into mpsc;
        //    on turn/completed it closes stdin so codex exits cleanly.
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();
        let session_id = thread_id.clone();
        let stderr_for_pump = stderr_handle.clone();

        // Emit a synthetic SessionStarted as the first event so the
        // consumer can rely on this contract (matches claude's
        // system/init forwarding).
        let first_event = anatta_core::AgentEvent {
            envelope: anatta_core::AgentEventEnvelope {
                session_id: session_id.clone(),
                timestamp: chrono::Utc::now(),
                backend: anatta_core::Backend::Codex,
                raw_uuid: None,
                parent_tool_use_id: None,
            },
            payload: anatta_core::AgentEventPayload::SessionStarted {
                cwd: cwd_str.to_string(),
                model: String::new(), // populated by upstream on demand
                tools_available: Vec::new(),
            },
        };
        events_tx
            .send(first_event)
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("consumer channel closed")))?;
        counter_for_task.fetch_add(1, Ordering::Relaxed);

        let pump_session_id = session_id.clone();
        let pump_envelope_session = session_id.clone();
        tokio::spawn(async move {
            let mut projector = AppServerProjector::new(pump_session_id);
            let mut stdin_holder = Some(stdin);
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
                                stderr_for_pump.append(line.as_bytes());
                                continue;
                            }
                        };
                        // Responses (carry `id`). turn/start (id=2)
                        // success: skip. Any error: surface as fatal
                        // AgentEvent::Error so the renderer shows the
                        // user what went wrong instead of timing out.
                        if value.get("id").is_some() {
                            if let Some(err) = value.get("error") {
                                let msg = err
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("(unknown)")
                                    .to_string();
                                let ev = anatta_core::AgentEvent {
                                    envelope: anatta_core::AgentEventEnvelope {
                                        session_id: pump_envelope_session.clone(),
                                        timestamp: chrono::Utc::now(),
                                        backend: anatta_core::Backend::Codex,
                                        raw_uuid: None,
                                        parent_tool_use_id: None,
                                    },
                                    payload: anatta_core::AgentEventPayload::Error {
                                        message: format!("codex JSON-RPC error: {msg}"),
                                        fatal: true,
                                    },
                                };
                                let _ = events_tx.send(ev).await;
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
                        let turn_done = method == "turn/completed";
                        for ev in projector.project(method, &params) {
                            if events_tx.send(ev).await.is_err() {
                                return;
                            }
                            counter_for_task.fetch_add(1, Ordering::Relaxed);
                        }
                        if turn_done {
                            // Close stdin so codex app-server exits.
                            drop(stdin_holder.take());
                            // Keep reading until EOF for any tail events.
                        }
                    }
                    Ok(None) => break, // EOF — server exited
                    Err(_) => break,
                }
            }
        });

        Ok(AgentSession::new(
            session_id,
            child,
            events_rx,
            stderr_handle,
            started_at,
            counter,
        ))
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
    let mut s = serde_json::to_string(&req).map_err(|e| {
        SpawnError::Io(std::io::Error::other(format!("serialize request: {e}")))
    })?;
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

/// Read lines until we see a response with the given id. Notifications
/// arriving in the meantime are silently dropped (they happen during
/// startup: mcpServer/startupStatus/updated, thread/started, etc).
/// Returns the response's `result` JSON.
async fn wait_for_response(
    reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
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
            // It's a notification or a response to a different id; drop.
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
