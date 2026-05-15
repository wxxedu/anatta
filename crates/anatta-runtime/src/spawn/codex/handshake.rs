//! Shared initialize → thread/start (or thread/resume) handshake.
//!
//! Used by both the one-shot [`super::launch`] flow and the long-lived
//! [`super::persistent`] flow. Returns a [`Handshake`] bundle with the
//! child's stdin/stdout reader, captured stderr buffer, the codex
//! `thread.id`, and the canonicalized cwd string.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::codex::app_server::wire::{
    ClientInfo, InitializeParams, InitializedParams, ThreadResumeParams, ThreadStartParams,
};
use crate::profile::CodexProfile;
use crate::spawn::SpawnError;
use crate::spawn::stderr_buf;

use super::pump::{wait_for_response, write_notification, write_request};

/// JSON-RPC request id for the `initialize` request.
const INITIALIZE_REQUEST_ID: i64 = 0;
/// JSON-RPC request id for the `thread/start` (or `thread/resume`) request.
const THREAD_REQUEST_ID: i64 = 1;

/// Result of a successful handshake. The caller owns the child and
/// drives stdin/stdout from here.
pub(super) struct Handshake {
    pub child: Child,
    pub stdin: ChildStdin,
    pub reader: Lines<BufReader<ChildStdout>>,
    pub stderr: stderr_buf::Handle,
    pub thread_id: String,
    pub cwd_str: String,
}

/// Spawn `codex app-server`, run `initialize` + `initialized` +
/// `thread/{start,resume}`, return the bundle of handles needed to
/// continue the protocol.
pub(super) async fn handshake(
    binary_path: &Path,
    profile: &CodexProfile,
    cwd: &Path,
    api_key: Option<&str>,
    resume: Option<&str>,
    policy: anatta_core::CodexPolicy,
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
    if policy.reviewer_armed {
        cmd.arg("-c").arg("approvals_reviewer=auto_review");
    }
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
                    approval_policy: policy.approval,
                    cwd: &cwd_str,
                    sandbox: policy.sandbox,
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
                    approval_policy: policy.approval,
                    cwd: &cwd_str,
                    sandbox: policy.sandbox,
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

fn extract_thread_id(result: serde_json::Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}
