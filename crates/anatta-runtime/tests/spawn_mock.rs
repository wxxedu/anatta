//! Spawn pipeline integration test using a mock backend.
//!
//! No real claude/codex auth required: we point ClaudeLaunch at a tiny
//! shell script that emits a hard-coded stream-json sequence, verify
//! the AgentSession surfaces session_id correctly, and confirm
//! AgentEvents flow through the projector.
//!
//! For the *real* end-to-end test (actually spawning claude / codex
//! against live API), see `spawn_e2e.rs` (gated `#[ignore]`).

#![cfg(feature = "spawn")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anatta_core::AgentEventPayload;
use anatta_runtime::profile::{ClaudeProfile, ClaudeProfileId};
use anatta_runtime::spawn::{ClaudeLaunch, Launchable};

/// Drop a tiny shell script in a tempdir that emits the given stdout
/// lines and exits 0. Returns the script path.
fn write_mock_script(dir: &std::path::Path, name: &str, stdout_lines: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let mut script = String::from("#!/bin/sh\n");
    for line in stdout_lines {
        // single-quote escape: each ' becomes '\''
        let escaped = line.replace('\'', "'\\''");
        script.push_str(&format!("printf '%s\\n' '{escaped}'\n"));
    }
    let mut f = std::fs::File::create(&path).expect("create mock script");
    f.write_all(script.as_bytes()).expect("write");
    let mut perms = f.metadata().unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

#[tokio::test]
async fn launch_extracts_session_id_from_first_init_event() {
    let tmp = tempfile::tempdir().unwrap();
    let anatta_root = tmp.path();

    // Real ClaudeProfile (sets up directory + symlinks).
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), anatta_root).unwrap();

    // Mock binary emits 3 stream-json lines: system/init (carries session_id),
    // assistant message, result.
    let lines = &[
        r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"mock-session-AAAA","tools":["Bash"],"mcp_servers":[],"model":"claude-test","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"test","output_style":"default","skills":[],"plugins":[],"uuid":"u-init"}"#,
        r#"{"type":"assistant","message":{"id":"m1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"mock-session-AAAA","uuid":"u-asst"}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"duration_api_ms":1,"num_turns":1,"result":"hi","stop_reason":"end_turn","session_id":"mock-session-AAAA","total_cost_usd":0.0,"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":""},"modelUsage":{},"permission_denials":[],"uuid":"u-result"}"#,
    ];
    let mock_bin = write_mock_script(tmp.path(), "mock-claude.sh", lines);

    let launch = ClaudeLaunch {
        profile,
        cwd: tmp.path().to_path_buf(),
        prompt: "hi".into(),
        resume: None,
        binary_path: mock_bin,
        provider: None,
        permission_level: anatta_core::PermissionLevel::Default,
    };

    let mut session = launch.launch().await.expect("launch");
    assert_eq!(session.session_id(), "mock-session-AAAA");

    // Drain events.
    let mut payloads = Vec::new();
    while let Some(e) = session.events().recv().await {
        payloads.push(e.payload);
    }

    // First was forwarded → SessionStarted; assistant → AssistantText + Usage; result → Usage + TurnCompleted.
    let kinds: Vec<&'static str> = payloads
        .iter()
        .map(|p| match p {
            AgentEventPayload::SessionStarted { .. } => "SessionStarted",
            AgentEventPayload::AssistantText { .. } => "AssistantText",
            AgentEventPayload::Usage { .. } => "Usage",
            AgentEventPayload::TurnCompleted { .. } => "TurnCompleted",
            _ => "Other",
        })
        .collect();

    assert!(
        kinds.contains(&"SessionStarted"),
        "expected SessionStarted, got {kinds:?}"
    );
    assert!(
        kinds.contains(&"AssistantText"),
        "expected AssistantText, got {kinds:?}"
    );
    assert!(
        kinds.contains(&"TurnCompleted"),
        "expected TurnCompleted, got {kinds:?}"
    );

    let exit = session.wait().await.expect("wait");
    assert_eq!(exit.exit_code, Some(0));
    assert!(
        exit.events_emitted >= 3,
        "events_emitted={}",
        exit.events_emitted
    );
}

#[tokio::test]
async fn launch_fails_when_binary_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), tmp.path()).unwrap();

    let launch = ClaudeLaunch {
        profile,
        cwd: tmp.path().to_path_buf(),
        prompt: "hi".into(),
        resume: None,
        binary_path: tmp.path().join("does-not-exist"),
        provider: None,
        permission_level: anatta_core::PermissionLevel::Default,
    };

    let err = launch.launch().await.expect_err("should fail");
    assert!(matches!(
        err,
        anatta_runtime::spawn::SpawnError::BinaryNotFound(_)
    ));
}

#[tokio::test]
async fn launch_fails_when_child_exits_without_emitting() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), tmp.path()).unwrap();

    // Empty script: exits 0 with no stdout.
    let mock_bin = write_mock_script(tmp.path(), "empty.sh", &[]);

    let launch = ClaudeLaunch {
        profile,
        cwd: tmp.path().to_path_buf(),
        prompt: "hi".into(),
        resume: None,
        binary_path: mock_bin,
        provider: None,
        permission_level: anatta_core::PermissionLevel::Default,
    };

    let err = launch
        .launch()
        .await
        .expect_err("should fail with no events");
    assert!(matches!(
        err,
        anatta_runtime::spawn::SpawnError::ChildExitedEarly { .. }
    ));
}

#[tokio::test]
async fn cancel_drops_stdin_and_returns_exit_info() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), tmp.path()).unwrap();

    // Script emits init, then loops reading stdin (so it stays alive
    // until we close stdin / kill).
    let init = r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"loop-session","tools":[],"mcp_servers":[],"model":"m","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"t","output_style":"default","skills":[],"plugins":[],"uuid":"u"}"#;
    let path = tmp.path().join("loop.sh");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nwhile read line; do :; done\n",
        init.replace('\'', "'\\''")
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let launch = ClaudeLaunch {
        profile,
        cwd: tmp.path().to_path_buf(),
        prompt: "hi".into(),
        resume: None,
        binary_path: path,
        provider: None,
        permission_level: anatta_core::PermissionLevel::Default,
    };

    let session = launch.launch().await.expect("launch");
    assert_eq!(session.session_id(), "loop-session");

    let exit = session
        .cancel_with_timeout(Duration::from_millis(500))
        .await
        .expect("cancel");
    // Either exited cleanly on stdin EOF (exit 0) or was SIGKILLed.
    assert!(
        exit.exit_code == Some(0) || exit.signal.is_some(),
        "unexpected exit: {exit:?}"
    );
}

#[tokio::test]
async fn launch_injects_provider_env_into_child_process() {
    use anatta_runtime::profile::ProviderEnv;

    let tmp = tempfile::tempdir().unwrap();
    let anatta_root = tmp.path();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), anatta_root).unwrap();

    // Mock script: dump env to $CLAUDE_CONFIG_DIR/env.dump, then emit a
    // single system/init line so the launch resolves successfully.
    let dir = tmp.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("claude-mock.sh");
    let script = r#"#!/bin/sh
env > "$CLAUDE_CONFIG_DIR/env.dump"
printf '%s\n' '{"type":"system","subtype":"init","cwd":"/tmp","session_id":"sess-fake-uuid","tools":[],"mcp_servers":[],"model":"x","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"test","output_style":"default","skills":[],"plugins":[],"uuid":"u-init"}'
"#;
    std::fs::write(&script_path, script).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let provider = ProviderEnv {
        vars: vec![
            (
                "ANTHROPIC_BASE_URL".into(),
                "https://api.test.example/anthropic".into(),
            ),
            ("ANTHROPIC_AUTH_TOKEN".into(), "sk-test-abc".into()),
            ("ANTHROPIC_MODEL".into(), "test-model-7b".into()),
            ("CLAUDE_CODE_EFFORT_LEVEL".into(), "max".into()),
        ],
    };

    let session = ClaudeLaunch {
        profile: profile.clone(),
        cwd: tmp.path().to_owned(),
        prompt: "hi".into(),
        resume: None,
        binary_path: script_path,
        provider: Some(provider),
        permission_level: anatta_core::PermissionLevel::Default,
    }
    .launch()
    .await
    .expect("launch should succeed against mock script");

    // Wait for child to exit so env.dump is fully flushed.
    let _ = session.wait().await.unwrap();

    let dumped =
        std::fs::read_to_string(profile.path.join("env.dump")).expect("env dump should exist");
    assert!(
        dumped.contains("ANTHROPIC_BASE_URL=https://api.test.example/anthropic"),
        "missing ANTHROPIC_BASE_URL in dump:\n{dumped}"
    );
    assert!(dumped.contains("ANTHROPIC_AUTH_TOKEN=sk-test-abc"));
    assert!(dumped.contains("ANTHROPIC_MODEL=test-model-7b"));
    assert!(dumped.contains("CLAUDE_CODE_EFFORT_LEVEL=max"));
}

#[tokio::test]
async fn launch_without_provider_does_not_inject_anthropic_env() {
    let tmp = tempfile::tempdir().unwrap();
    let anatta_root = tmp.path();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), anatta_root).unwrap();

    let dir = tmp.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("claude-mock.sh");
    let script = r#"#!/bin/sh
env > "$CLAUDE_CONFIG_DIR/env.dump"
printf '%s\n' '{"type":"system","subtype":"init","cwd":"/tmp","session_id":"sess-fake-uuid","tools":[],"mcp_servers":[],"model":"x","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"test","output_style":"default","skills":[],"plugins":[],"uuid":"u-init"}'
"#;
    std::fs::write(&script_path, script).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let session = ClaudeLaunch {
        profile: profile.clone(),
        cwd: tmp.path().to_owned(),
        prompt: "hi".into(),
        resume: None,
        binary_path: script_path,
        provider: None,
        permission_level: anatta_core::PermissionLevel::Default,
    }
    .launch()
    .await
    .expect("launch");
    let _ = session.wait().await.unwrap();

    let dumped = std::fs::read_to_string(profile.path.join("env.dump")).unwrap();
    // CLAUDE_CONFIG_DIR is always set by the spawn code; we assert that
    // ANTHROPIC_AUTH_TOKEN is NOT — that's the OAuth path.
    assert!(
        dumped.contains("CLAUDE_CONFIG_DIR="),
        "CLAUDE_CONFIG_DIR should always be set"
    );
    assert!(
        !dumped.contains("ANTHROPIC_AUTH_TOKEN="),
        "ANTHROPIC_AUTH_TOKEN must NOT be set in OAuth path:\n{dumped}"
    );
    assert!(
        !dumped.contains("ANTHROPIC_BASE_URL="),
        "ANTHROPIC_BASE_URL must NOT be set in OAuth path"
    );
}
