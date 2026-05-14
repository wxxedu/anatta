//! In-process unit tests for the interactive PTY backend.
//!
//! Real-PTY spawn tests live in `spawn_e2e.rs` (ignored; hits the
//! user's installed claude binary). These tests stay process-local and
//! run every CI build.

#![cfg(feature = "spawn")]

use anatta_runtime::spawn::encode_prompt_for_test;

#[test]
fn encode_prompt_wraps_in_bracketed_paste_and_terminates_with_cr() {
    let bytes = encode_prompt_for_test("Say only OK");
    assert_eq!(
        bytes,
        b"\x1b[200~Say only OK\x1b[201~\r".to_vec(),
        "expected bracketed-paste start, prompt, bracketed-paste end, CR",
    );
}

#[test]
fn encode_prompt_passes_through_newlines_inside_paste_bracket() {
    let bytes = encode_prompt_for_test("line one\nline two");
    assert_eq!(bytes, b"\x1b[200~line one\nline two\x1b[201~\r".to_vec(),);
}

use anatta_core::AgentEventPayload;
use anatta_runtime::spawn::run_tail_for_test;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn tail_emits_assistant_text_then_turn_completed_then_closes_on_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("session.jsonl");
    std::fs::write(&path, "").unwrap();

    let (tx, mut rx) = mpsc::channel(16);
    let path_for_task = path.clone();
    let handle =
        tokio::spawn(
            async move { run_tail_for_test(path_for_task, tx, "sess-1".to_owned()).await },
        );

    let assistant_line = r#"{"type":"assistant","uuid":"u1","parentUuid":null,"sessionId":"sess-1","timestamp":"2026-05-14T00:00:00Z","cwd":"/tmp","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","message":{"id":"m1","model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"text","text":"OK"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#;
    let turn_done_line = r#"{"type":"system","uuid":"u2","parentUuid":"u1","sessionId":"sess-1","timestamp":"2026-05-14T00:00:01Z","cwd":"/tmp","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","subtype":"turn_duration","durationMs":1234,"messageCount":2}"#;

    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{assistant_line}").unwrap();
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{turn_done_line}").unwrap();
    }

    let mut payloads = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("tail produced events within 2s")
    {
        payloads.push(ev.payload);
    }

    assert!(matches!(
        payloads.first().unwrap(),
        AgentEventPayload::AssistantText { .. }
    ));
    assert!(matches!(
        payloads.last().unwrap(),
        AgentEventPayload::TurnCompleted { .. }
    ));
    handle.await.unwrap();
}
