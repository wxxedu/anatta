//! Fixture-based unit tests for codex → claude transcoding.

use std::fs;

use serde_json::Value;

use crate::transcode::{Engine, TranscodeInput, transcode_to};

fn write_src(tmp: &std::path::Path, lines: &[&str]) -> std::path::PathBuf {
    let p = tmp.join("source.jsonl");
    let mut content = String::new();
    for l in lines {
        content.push_str(l);
        content.push('\n');
    }
    fs::write(&p, content).unwrap();
    p
}

fn read_view(view_dir: &std::path::Path) -> Vec<Value> {
    let raw = fs::read_to_string(view_dir.join("events.jsonl")).unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn etype(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

fn standard_codex_preamble() -> Vec<String> {
    vec![
        r#"{"type":"session_meta","timestamp":"2026-05-13T12:00:00Z","payload":{"id":"019c-src","cwd":"/work","originator":"codex_exec","cli_version":"0.125.0","source":"exec","model_provider":"openai"}}"#.to_owned(),
        r#"{"type":"turn_context","timestamp":"2026-05-13T12:00:00Z","payload":{"cwd":"/work","model":"gpt-5","approval_policy":"never","sandbox_policy":{"type":"danger_full_access"}}}"#.to_owned(),
    ]
}

#[test]
fn first_line_emitted_is_system_init_with_view_id() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","timestamp":"2026-05-13T12:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");

    let out = transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "019c-src",
            conversation_cwd: "/conv-cwd",
        },
        &view,
    )
    .unwrap();

    assert_eq!(out.view_engine_session_id, "anatta-view-019c-src-claude");

    let emitted = read_view(&view);
    assert_eq!(etype(&emitted[0]), "system");
    assert_eq!(
        emitted[0].get("subtype").and_then(Value::as_str),
        Some("init")
    );
    assert_eq!(
        emitted[0].get("model").and_then(Value::as_str),
        Some("gpt-5")
    );
    // parentUuid on system/init should be null.
    assert_eq!(emitted[0].get("parentUuid"), Some(&Value::Null));
    assert_eq!(
        emitted[0].get("sessionId").and_then(Value::as_str),
        Some("anatta-view-019c-src-claude")
    );
}

#[test]
fn codex_user_message_becomes_claude_user_text() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"what color"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let emitted = read_view(&view);
    // [0] system/init, [1] user
    let u = &emitted[1];
    assert_eq!(etype(u), "user");
    assert_eq!(u["message"]["content"][0]["type"].as_str(), Some("text"));
    assert_eq!(
        u["message"]["content"][0]["text"].as_str(),
        Some("what color")
    );
}

#[test]
fn codex_assistant_message_becomes_claude_assistant_text() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let emitted = read_view(&view);
    let a = &emitted[1];
    assert_eq!(etype(a), "assistant");
    assert_eq!(a["message"]["content"][0]["text"].as_str(), Some("answer"));
}

#[test]
fn codex_reasoning_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"type":"text","text":"thought"}],"summary":[],"encrypted_content":"ENC_X"}}"#.to_owned());
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let raw = fs::read_to_string(view.join("events.jsonl")).unwrap();
    assert!(!raw.contains("ENC_X"), "encrypted_content must not leak");
    assert!(!raw.contains("\"thought\""), "reasoning text must not leak");
    let emitted = read_view(&view);
    assert_eq!(
        emitted.len(),
        2,
        "system/init + assistant; reasoning dropped"
    );
}

#[test]
fn function_call_becomes_tool_use_with_namespaced_id() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_abc","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let emitted = read_view(&view);
    let asst = &emitted[1];
    assert_eq!(etype(asst), "assistant");
    let block = &asst["message"]["content"][0];
    assert_eq!(block["type"].as_str(), Some("tool_use"));
    assert_eq!(block["id"].as_str(), Some("anatta-cx-call_abc"));
    assert_eq!(block["name"].as_str(), Some("shell"));
    assert_eq!(block["input"]["cmd"].as_str(), Some("ls"));
}

#[test]
fn function_call_output_becomes_tool_result_with_paired_id() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_abc","output":"file1\nfile2"}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let emitted = read_view(&view);
    let user = &emitted[1];
    assert_eq!(etype(user), "user");
    let block = &user["message"]["content"][0];
    assert_eq!(block["type"].as_str(), Some("tool_result"));
    assert_eq!(block["tool_use_id"].as_str(), Some("anatta-cx-call_abc"));
    assert_eq!(block["content"].as_str(), Some("file1\nfile2"));
}

#[test]
fn linear_dag_parent_pointer_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}}"#.to_owned());
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let emitted = read_view(&view);
    // [0] init parent=null, [1] user parent=init.uuid, [2] assistant parent=user.uuid
    assert_eq!(emitted[0]["parentUuid"], Value::Null);
    let init_uuid = emitted[0]["uuid"].as_str().unwrap().to_owned();
    let user_uuid = emitted[1]["uuid"].as_str().unwrap().to_owned();
    assert_eq!(emitted[1]["parentUuid"].as_str(), Some(init_uuid.as_str()));
    assert_eq!(emitted[2]["parentUuid"].as_str(), Some(user_uuid.as_str()));
}

#[test]
fn developer_role_messages_are_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions ...>"}]}}"#.to_owned());
    lines.push(r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real user"}]}}"#.to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap();
    let raw = fs::read_to_string(view.join("events.jsonl")).unwrap();
    assert!(
        !raw.contains("permissions"),
        "developer message must be dropped"
    );
    let emitted = read_view(&view);
    // [0] system/init, [1] user (developer dropped)
    assert_eq!(emitted.len(), 2);
    assert_eq!(
        emitted[1]["message"]["content"][0]["text"].as_str(),
        Some("real user")
    );
}

#[test]
fn malformed_source_returns_parse_error_and_leaves_no_view() {
    let tmp = tempfile::tempdir().unwrap();
    let mut lines = standard_codex_preamble();
    lines.push("{not json".to_owned());
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let src = write_src(tmp.path(), &line_refs);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    let err = transcode_to(
        Engine::Claude,
        TranscodeInput {
            source_engine: Engine::Codex,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/c",
        },
        &view,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        crate::transcode::TranscodeError::Parse { line: 2, .. }
    ));
    assert!(!view.exists());
}
