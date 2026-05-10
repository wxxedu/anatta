//! Shared pipeline scaffolding: spawn child, wire stdout → AgentEvents,
//! capture stderr, return handles.
//!
//! Per-backend `launch()` functions provide a `line_to_events: FnMut(&str) -> Vec<AgentEvent>`
//! closure. The reader task pulls each stdout line, runs the closure
//! (sync — runs the parser + projector), then awaits-sends each event.
//! This keeps the closure ergonomic (just transformation) while still
//! using async backpressure on the channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anatta_core::AgentEvent;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::stderr_buf;
use super::SpawnError;

pub struct PipelineHandles {
    pub child: Child,
    pub events_rx: mpsc::Receiver<AgentEvent>,
    pub stderr: stderr_buf::Handle,
    pub events_emitted: Arc<AtomicU64>,
}

/// Spawn `cmd` with stdin/stdout/stderr piped + kill_on_drop, then start
/// background tasks: stdout reader (running `line_to_events` per line)
/// and stderr capture (rolling 64 KB buffer).
pub async fn spawn_with_pipeline<F>(
    mut cmd: Command,
    mut line_to_events: F,
) -> Result<PipelineHandles, SpawnError>
where
    F: FnMut(&str) -> Vec<AgentEvent> + Send + 'static,
{
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(SpawnError::ProcessSpawn)?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("child had no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SpawnError::Io(std::io::Error::other("child had no stderr")))?;

    let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);
    let counter = Arc::new(AtomicU64::new(0));

    // stdout reader task
    let counter_for_stdout = counter.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let evts = line_to_events(&line);
                    for e in evts {
                        if events_tx.send(e).await.is_err() {
                            return;
                        }
                        counter_for_stdout.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(None) => break, // EOF
                Err(_) => break,
            }
        }
    });

    // stderr capture task
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

    Ok(PipelineHandles {
        child,
        events_rx,
        stderr: stderr_handle,
        events_emitted: counter,
    })
}
