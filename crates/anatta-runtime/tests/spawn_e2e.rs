//! Real end-to-end spawn test against the user's installed claude /
//! codex binaries, using the user's existing profile auth.
//!
//! Marked `#[ignore]` because this test:
//!   * requires `claude` / `codex` to be installed
//!   * requires the user to be logged in (uses ~/.claude / ~/.codex)
//!   * makes a real API call (consumes quota)
//!   * pollutes the user's session history with a small "say only OK" run
//!
//! Run explicitly:
//!     cargo test -p anatta-runtime --features spawn --test spawn_e2e -- --ignored --nocapture
//!
//! NOTE for the interactive PTY tests
//! (`launch_real_claude_interactive_emits_session_started_assistant_completion`
//! and `interactive_cancel_closes_turn_channel`): macOS keychain access
//! is gated per-process, so these will fail with "Not logged in" if run
//! from inside a Claude Code session subprocess. Run from a regular
//! terminal:
//!
//! ```bash
//! cargo test -p anatta-runtime --features spawn --test spawn_e2e -- \
//!     --ignored --nocapture launch_real_claude_interactive interactive_cancel
//! ```

#![cfg(feature = "spawn")]

use std::path::PathBuf;
use std::time::Duration;

use anatta_core::AgentEventPayload;
use anatta_runtime::profile::{ClaudeProfile, ClaudeProfileId, CodexProfile, CodexProfileId};
use anatta_runtime::spawn::{
    ClaudeInteractiveLaunch, ClaudeInteractiveSession, ClaudeLaunch, CodexLaunch, Launchable,
};

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn locate_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[tokio::test]
#[ignore = "real claude API call; requires logged-in ~/.claude"]
async fn launch_real_claude_emits_session_started_assistant_completion() {
    let bin = locate_binary("claude").expect("claude binary not on PATH");
    let claude_dir = home().join(".claude");
    assert!(
        claude_dir.is_dir(),
        "no ~/.claude found; log in via `claude /login` first"
    );

    // Borrow the user's already-authenticated profile dir for this run.
    // We're not creating an anatta-managed profile (that's the CLI's job),
    // we're just pointing spawn at a CLAUDE_CONFIG_DIR with valid auth.
    let profile = ClaudeProfile {
        id: ClaudeProfileId::new(), // dummy id; spawn only reads .path
        path: claude_dir,
    };

    let cwd = tempfile::tempdir().expect("tempdir");
    let launch = ClaudeLaunch {
        profile,
        cwd: cwd.path().to_path_buf(),
        prompt: "Say only OK and nothing else".into(),
        resume: None,
        binary_path: bin,
        provider: None,
    };

    let mut session = launch.launch().await.expect("launch");
    let session_id = session.session_id().to_owned();
    eprintln!("real claude session_id = {session_id}");
    assert!(!session_id.is_empty(), "missing session_id");

    let mut saw_session_started = false;
    let mut saw_assistant_text = false;
    let mut saw_turn_completed = false;
    let mut all_text = String::new();

    while let Some(evt) = session.events().recv().await {
        match &evt.payload {
            AgentEventPayload::SessionStarted { model, .. } => {
                saw_session_started = true;
                eprintln!("SessionStarted model={model}");
            }
            AgentEventPayload::AssistantText { text } => {
                saw_assistant_text = true;
                all_text.push_str(text);
                eprintln!("AssistantText: {text:?}");
            }
            AgentEventPayload::AssistantTextDelta { text_so_far, .. } => {
                eprintln!("...delta {text_so_far:?}");
            }
            AgentEventPayload::TurnCompleted {
                stop_reason,
                is_error,
            } => {
                saw_turn_completed = true;
                eprintln!("TurnCompleted stop_reason={stop_reason:?} is_error={is_error}");
            }
            other => eprintln!("(other) {other:?}"),
        }
    }

    let exit = session.wait().await.expect("wait");
    eprintln!(
        "exit code={:?} duration={:?} events={}",
        exit.exit_code, exit.duration, exit.events_emitted
    );
    if !exit.stderr_tail.is_empty() {
        eprintln!(
            "--- stderr tail ---\n{}\n--- end stderr ---",
            exit.stderr_tail
        );
    }

    assert!(saw_session_started, "no SessionStarted event");
    assert!(saw_assistant_text, "no AssistantText event");
    assert!(saw_turn_completed, "no TurnCompleted event");

    // If the AssistantText says "Not logged in", that's an auth issue at
    // the spawn-environment / keychain-ACL layer (not a pipeline bug).
    // Surface it as an instructive panic rather than a noise assertion failure.
    if all_text.to_ascii_lowercase().contains("not logged in") {
        panic!(
            "claude reported 'Not logged in' from inside the spawn pipeline.\n\
             The pipeline itself works (events arrived in the right order),\n\
             but claude can't access the keychain from this subprocess.\n\
             To run this test against a real session, either:\n\
             - run from an environment where keychain access is pre-granted, or\n\
             - finish the anatta CLI login flow and use an anatta-managed profile\n\
             AssistantText was: {all_text:?}"
        );
    }
    assert!(
        all_text.to_ascii_lowercase().contains("ok"),
        "assistant text did not contain OK; got {all_text:?}"
    );

    let _ = exit.exit_code;

    drop(cwd);
    let _ = Duration::from_secs(0); // silence unused-import lint
}

#[tokio::test]
#[ignore = "real claude API call; requires logged-in ~/.claude"]
async fn launch_real_claude_interactive_emits_session_started_assistant_completion() {
    let bin = locate_binary("claude").expect("claude binary not on PATH");
    let claude_dir = home().join(".claude");
    assert!(
        claude_dir.is_dir(),
        "no ~/.claude found; log in via `claude /login` first"
    );

    let profile = ClaudeProfile {
        id: ClaudeProfileId::new(),
        path: claude_dir,
    };
    let cwd_tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize: macOS resolves /tmp → /private/tmp; claude's JSONL
    // path uses the canonical form.
    let cwd = std::fs::canonicalize(cwd_tmp.path()).expect("canonicalize");

    let launch = ClaudeInteractiveLaunch {
        profile,
        cwd,
        resume: None,
        binary_path: bin,
        provider: None,
        model: None,
        // OAuth-based ~/.claude profiles require keychain access, which
        // `--bare` disables. The CLI's `build_claude_interactive` derives
        // `bare` from `record.auth_method`; this test constructs the launch
        // directly, so it must hard-code the OAuth-compatible value.
        bare: false,
    };

    let session = ClaudeInteractiveSession::open(launch).await.expect("open");
    let session_id = session.session_id().to_owned();
    eprintln!("interactive session_id = {session_id}");
    assert!(!session_id.is_empty());

    let mut turn = session
        .send_turn("Say only OK and nothing else")
        .await
        .expect("send_turn");

    let mut saw_session_started = false;
    let mut saw_assistant_text = false;
    let mut saw_turn_completed = false;
    let mut all_text = String::new();
    while let Some(ev) = turn.events().recv().await {
        match &ev.payload {
            AgentEventPayload::SessionStarted { .. } => saw_session_started = true,
            AgentEventPayload::AssistantText { text } => {
                saw_assistant_text = true;
                all_text.push_str(text);
            }
            AgentEventPayload::TurnCompleted { .. } => saw_turn_completed = true,
            _ => {}
        }
    }
    assert!(saw_session_started, "no SessionStarted");
    assert!(saw_assistant_text, "no AssistantText");
    assert!(saw_turn_completed, "no TurnCompleted");
    assert!(
        all_text.to_ascii_lowercase().contains("ok"),
        "expected reply containing 'OK': {all_text:?}"
    );

    let exit = session.close().await.expect("close");
    eprintln!(
        "interactive exit code={:?} duration={:?} events={}",
        exit.exit_code, exit.duration, exit.events_emitted
    );
}

#[tokio::test]
#[ignore = "real claude API call; requires logged-in ~/.claude"]
async fn interactive_cancel_closes_turn_channel() {
    use anatta_runtime::spawn::{ClaudeInteractiveLaunch, ClaudeInteractiveSession};
    use std::time::Duration;

    let bin = locate_binary("claude").expect("claude binary not on PATH");
    let claude_dir = home().join(".claude");
    let profile = ClaudeProfile {
        id: ClaudeProfileId::new(),
        path: claude_dir,
    };
    let cwd = std::fs::canonicalize(tempfile::tempdir().unwrap().path()).unwrap();

    let session = ClaudeInteractiveSession::open(ClaudeInteractiveLaunch {
        profile,
        cwd,
        resume: None,
        binary_path: bin,
        provider: None,
        model: None,
        // OAuth-based ~/.claude profiles require keychain access, which
        // `--bare` disables. The CLI's `build_claude_interactive` derives
        // `bare` from `record.auth_method`; this test constructs the launch
        // directly, so it must hard-code the OAuth-compatible value.
        bare: false,
    })
    .await
    .expect("open");

    let mut turn = session
        .send_turn("Count slowly from 1 to 100, one number per line, with a thoughtful sentence after each")
        .await
        .expect("send_turn");

    // Let some assistant output start, then cancel.
    let _ = tokio::time::timeout(Duration::from_secs(3), turn.events().recv()).await;
    session
        .interrupt_handle()
        .interrupt()
        .await
        .expect("interrupt");

    // Channel must close within a reasonable grace.
    let drain = async { while turn.events().recv().await.is_some() {} };
    tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .expect("turn channel did not close within 10s after interrupt");
}

#[tokio::test]
#[ignore = "real codex API call; requires logged-in ~/.codex"]
async fn launch_real_codex_emits_session_started_assistant_completion() {
    let bin = locate_binary("codex").expect("codex binary not on PATH");
    let codex_dir = home().join(".codex");
    assert!(
        codex_dir.is_dir(),
        "no ~/.codex found; log in via `codex login` first"
    );

    let profile = CodexProfile {
        id: CodexProfileId::new(),
        path: codex_dir,
    };

    let cwd = tempfile::tempdir().expect("tempdir");
    let launch = CodexLaunch {
        profile,
        cwd: cwd.path().to_path_buf(),
        prompt: "Say only OK and nothing else".into(),
        resume: None,
        binary_path: bin,
        api_key: None,
    };

    let mut session = launch.launch().await.expect("launch");
    let session_id = session.session_id().to_owned();
    eprintln!("real codex session_id = {session_id}");
    assert!(!session_id.is_empty(), "missing session_id");

    let mut saw_session_started = false;
    let mut saw_assistant_text = false;
    let mut saw_turn_completed = false;
    let mut all_text = String::new();

    while let Some(evt) = session.events().recv().await {
        match &evt.payload {
            AgentEventPayload::SessionStarted { .. } => saw_session_started = true,
            AgentEventPayload::AssistantText { text } => {
                saw_assistant_text = true;
                all_text.push_str(text);
                eprintln!("AssistantText: {text:?}");
            }
            AgentEventPayload::TurnCompleted { is_error, .. } => {
                saw_turn_completed = true;
                eprintln!("TurnCompleted is_error={is_error}");
            }
            other => eprintln!("(other) {other:?}"),
        }
    }

    let exit = session.wait().await.expect("wait");
    eprintln!(
        "exit code={:?} duration={:?}",
        exit.exit_code, exit.duration
    );

    assert!(saw_session_started, "no SessionStarted");
    assert!(saw_assistant_text, "no AssistantText");
    assert!(saw_turn_completed, "no TurnCompleted");
    assert!(
        all_text.to_ascii_lowercase().contains("ok"),
        "assistant text did not contain OK; got {all_text:?}"
    );
}
