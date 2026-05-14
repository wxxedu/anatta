//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

use std::path::PathBuf;
use std::time::Duration;

use anatta_core::{AgentEvent, AgentEventPayload, ProjectionContext, Projector};
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::mpsc;

use crate::claude::history::ClaudeEvent;
use crate::claude::HistoryProjector;

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
