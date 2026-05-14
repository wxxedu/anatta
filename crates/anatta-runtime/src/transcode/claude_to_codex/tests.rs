//! Fixture-based unit tests for claude → codex transcoding.
//!
//! Each test feeds a small claude jsonl, runs transcode_to, and asserts
//! shape of the emitted codex view. Tests focus on event-category
//! coverage; cross-category interactions live in integration tests.

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
    let p = view_dir.join("rollout.jsonl");
    let raw = fs::read_to_string(p).unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn payload(line: &Value) -> &Value {
    line.get("payload").expect("line must have payload")
}

fn item_type(line: &Value) -> &str {
    line.get("type").and_then(Value::as_str).unwrap_or("")
}

fn payload_type(line: &Value) -> &str {
    payload(line)
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn preamble_emits_session_meta_then_turn_context() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(tmp.path(), &[]);
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");

    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src-cc-session",
            conversation_cwd: "/tmp/work",
        },
        &view,
    )
    .unwrap();

    let lines = read_view(&view);
    assert_eq!(lines.len(), 2, "preamble is exactly 2 lines");
    assert_eq!(item_type(&lines[0]), "session_meta");
    assert_eq!(item_type(&lines[1]), "turn_context");
    assert_eq!(
        payload(&lines[0])
            .get("id")
            .and_then(Value::as_str)
            .unwrap(),
        "anatta-view-src-cc-session-codex"
    );
    assert_eq!(
        payload(&lines[0])
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap(),
        "/tmp/work"
    );
}

#[test]
fn user_text_becomes_message_role_user_input_text() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"S","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    let user_line = &lines[2]; // 0=meta, 1=turn_ctx, 2=user
    assert_eq!(item_type(user_line), "response_item");
    assert_eq!(payload_type(user_line), "message");
    assert_eq!(
        payload(user_line).get("role").and_then(Value::as_str),
        Some("user")
    );
    let c0 = &payload(user_line).get("content").unwrap()[0];
    assert_eq!(c0.get("type").and_then(Value::as_str), Some("input_text"));
    assert_eq!(c0.get("text").and_then(Value::as_str), Some("hi"));
}

#[test]
fn assistant_text_becomes_message_role_assistant_output_text() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"S","message":{"role":"assistant","content":[{"type":"text","text":"hello back"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    let asst = &lines[2];
    assert_eq!(payload_type(asst), "message");
    assert_eq!(
        payload(asst).get("role").and_then(Value::as_str),
        Some("assistant")
    );
    let c0 = &payload(asst).get("content").unwrap()[0];
    assert_eq!(c0.get("type").and_then(Value::as_str), Some("output_text"));
    assert_eq!(c0.get("text").and_then(Value::as_str), Some("hello back"));
}

#[test]
fn assistant_thinking_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"S","message":{"role":"user","content":[{"type":"text","text":"q"}]}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"S","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"SIG_X"}]}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":"S","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();

    // Read raw to make sure SIG_X never appears anywhere in the view.
    let raw = fs::read_to_string(view.join("rollout.jsonl")).unwrap();
    assert!(
        !raw.contains("SIG_X"),
        "thinking signature must not appear in view"
    );
    assert!(
        !raw.contains("\"thinking\""),
        "thinking content must be dropped"
    );
    // 2 preamble + 1 user + 1 assistant text (thinking dropped)
    let lines = read_view(&view);
    assert_eq!(lines.len(), 4, "thinking-only line removed, others kept");
}

#[test]
fn tool_use_becomes_function_call_with_namespaced_id() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"assistant","uuid":"a1","sessionId":"S","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01ABC","name":"Bash","input":{"command":"ls"}}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    let fcall = &lines[2];
    assert_eq!(payload_type(fcall), "function_call");
    assert_eq!(
        payload(fcall).get("call_id").and_then(Value::as_str),
        Some("anatta-cc-toolu_01ABC")
    );
    assert_eq!(
        payload(fcall).get("name").and_then(Value::as_str),
        Some("Bash")
    );
    let args = payload(fcall)
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap();
    let parsed: Value = serde_json::from_str(args).unwrap();
    assert_eq!(parsed["command"], "ls");
}

#[test]
fn tool_result_becomes_function_call_output_with_paired_id() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","uuid":"u2","sessionId":"S","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01ABC","content":"file1\nfile2"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    let out = &lines[2];
    assert_eq!(payload_type(out), "function_call_output");
    assert_eq!(
        payload(out).get("call_id").and_then(Value::as_str),
        Some("anatta-cc-toolu_01ABC"),
        "id mapping must pair with the tool_use call"
    );
    assert_eq!(
        payload(out).get("output").and_then(Value::as_str),
        Some("file1\nfile2")
    );
}

#[test]
fn tool_result_with_array_content_concatenates_text_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","sessionId":"S","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_X","content":[{"type":"text","text":"part one "},{"type":"text","text":"part two"}]}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    let out = &lines[2];
    assert_eq!(
        payload(out).get("output").and_then(Value::as_str),
        Some("part one part two")
    );
}

#[test]
fn compact_summary_becomes_compacted_line() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","parentUuid":null,"sessionId":"S","compactMetadata":{"trigger":"manual"}}"#,
            r#"{"type":"user","isCompactSummary":true,"uuid":"u9","parentUuid":"cb1","sessionId":"S","message":{"role":"user","content":[{"type":"text","text":"<previous summary text>"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    // 2 preamble + 1 compacted (compact_boundary was dropped, isCompactSummary user → compacted)
    assert_eq!(
        lines.len(),
        3,
        "compact_boundary suppressed, summary emitted as compacted"
    );
    let compacted = &lines[2];
    assert_eq!(item_type(compacted), "compacted");
    assert_eq!(
        payload(compacted).get("message").and_then(Value::as_str),
        Some("<previous summary text>")
    );
}

#[test]
fn unknown_event_types_are_dropped_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"some_future_kind","data":42}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"after unknown"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let lines = read_view(&view);
    assert_eq!(lines.len(), 3, "unknown kind dropped, known one kept");
    assert_eq!(payload_type(&lines[2]), "message");
}

#[test]
fn malformed_line_returns_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"ok"}]}}"#,
            r#"{this is not valid json"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");
    let err = transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        crate::transcode::TranscodeError::Parse { line: 1, .. }
    ));
    // On error, view dir should not be left half-built.
    assert!(
        !view.exists(),
        "view dir must not exist after transcode error"
    );
}

#[test]
fn atomic_replace_overwrites_existing_view_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let src = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
        ],
    );
    let sidecar = tmp.path().join("sidecar");
    let view = tmp.path().join("view");

    // First transcode succeeds, view exists.
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    assert!(view.exists());

    // Second transcode with different content → view dir replaced atomically.
    let src2 = write_src(
        tmp.path(),
        &[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
        ],
    );
    transcode_to(
        Engine::Codex,
        TranscodeInput {
            source_engine: Engine::Claude,
            source_events_jsonl: &src2,
            source_sidecar_dir: &sidecar,
            source_engine_session_id: "src",
            conversation_cwd: "/tmp",
        },
        &view,
    )
    .unwrap();
    let raw = fs::read_to_string(view.join("rollout.jsonl")).unwrap();
    assert!(raw.contains("second"));
    assert!(!raw.contains("first"));
}
