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
    assert_eq!(
        bytes,
        b"\x1b[200~line one\nline two\x1b[201~\r".to_vec(),
    );
}
