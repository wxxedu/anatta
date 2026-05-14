//! Reader-loop bodies + JSON-RPC I/O helpers + event-builder helpers
//! shared by both the one-shot and persistent codex flows.
//!
//! Two reader loops live here:
//!
//! * [`run_pump`] — one-shot. Drains notifications, projects them into
//!   `AgentEvent`s on the consumer channel, and closes stdin as soon
//!   as `turn/completed` is observed so the app-server shuts down.
//!
//! * [`persistent_reader_loop`] — long-lived. Routes notifications to
//!   the currently-active turn (if any), handles error responses for
//!   that turn's request id, and tears the turn down when
//!   `turn/completed` arrives (without closing the session).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anatta_core::AgentEvent;
use tokio::io::{AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex};

use crate::codex::app_server::wire::{OutgoingNotification, OutgoingRequest};
use crate::codex::app_server::AppServerProjector;
use crate::spawn::stderr_buf;
use crate::spawn::SpawnError;

use super::persistent::ActiveTurn;

// ──────────────────────────────────────────────────────────────────────
// Reader loops
// ──────────────────────────────────────────────────────────────────────

/// One-shot pump. Drains lines, projects notifications, sends events.
/// On `turn_done_pred` returning true (i.e. turn/completed observed),
/// closes stdin so codex exits cleanly.
pub(super) async fn run_pump<F, E>(
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
    let session_id = projector.session_id().to_owned();
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
pub(super) async fn persistent_reader_loop(
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
                            let ActiveTurn { events_tx, .. } = at.take().unwrap();
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

pub(super) async fn write_request<P: serde::Serialize>(
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

pub(super) async fn write_notification<P: serde::Serialize>(
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

pub(super) async fn wait_for_response(
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

// ──────────────────────────────────────────────────────────────────────
// Event builders
// ──────────────────────────────────────────────────────────────────────

/// Emit a synthetic `SessionStarted` AgentEvent on the consumer channel.
/// Mirrors claude's `system/init` forwarding so the consumer contract is
/// uniform across backends.
pub(super) async fn push_synthetic_session_started(
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

pub(super) fn make_error_event(thread_id: &str, message: &str) -> AgentEvent {
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
