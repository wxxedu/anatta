//! End-to-end conversation lifecycle test.
//!
//! Exercises the full tier-1 path through real primitives (no claude
//! subprocess, no API tokens):
//!
//!   1. Create a conversation + segment 0 under profile A (a-native)
//!   2. Simulate first turn: claude writes JSONL into the working area
//!   3. Absorb → verify central segment 0's events.jsonl matches
//!   4. Multiple turns: append → absorb → verify offset advances
//!   5. Profile swap to profile B (a-compat, lax) → new segment, render with Verbatim
//!   6. Verify working file under profile B is identical to central segment 0
//!   7. Turn under profile B with a thinking block + signature
//!   8. Absorb → verify segment 1's events
//!   9. Swap back to profile A (a-native, strict) → new segment 2, render with
//!      Verbatim for seg 0, StripReasoning for seg 1
//!  10. Verify segment 1's thinking signature is stripped in working file
//!  11. Crash-recovery: simulate offset DB update failure → re-run absorb,
//!      no duplicate bytes appended
//!  12. Finalize session: working file deleted, segment offsets reset
//!
//! This is the most realistic test we can run without spending API tokens.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anatta_runtime::conversation::{
    AbsorbInput, AbsorbOutcome, PriorSegmentInput, RenderOutcome, absorb_after_turn,
    render_into_working,
};
use anatta_runtime::conversation::{encode_cwd, working_jsonl_path, working_sidecar_dir};
use anatta_runtime::profile::{
    BackendKind, Family, SegmentRenderPolicy, family_of, min_policy_for,
};

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

fn user_msg(uuid: &str, parent: Option<&str>, text: &str, sess: &str) -> String {
    let parent_field = match parent {
        Some(p) => format!("\"{p}\""),
        None => "null".to_string(),
    };
    format!(
        r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_field},"sessionId":"{sess}","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

fn assistant_text(uuid: &str, parent: &str, text: &str, sess: &str) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","sessionId":"{sess}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

fn assistant_thinking(uuid: &str, parent: &str, text: &str, sig: &str, sess: &str) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","sessionId":"{sess}","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{text}","signature":"{sig}"}}]}}}}"#
    )
}

fn append_lines(path: &std::path::Path, lines: &[String]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
}

// Mimic the orchestration: a "segment" is a (id, profile_dir, source_family,
// last_absorbed_bytes, render_initial_bytes, ended) row plus on-disk paths.
#[allow(dead_code)] // `profile_dir` documents the row shape; current tests don't read it
struct Segment {
    id: String,
    profile_dir: PathBuf,
    central_dir: PathBuf,
    source_family: Family,
    last_absorbed_bytes: u64,
    render_initial_bytes: u64,
    ended: bool,
}

impl Segment {
    fn central_events(&self) -> PathBuf {
        self.central_dir.join("events.jsonl")
    }
    fn central_sidecar(&self) -> PathBuf {
        self.central_dir.join("sidecar")
    }
}

fn central_dir(anatta_home: &std::path::Path, conv_id: &str, seg_id: &str) -> PathBuf {
    anatta_home
        .join("conversations")
        .join(conv_id)
        .join("segments")
        .join(seg_id)
}

fn working_paths(profile_dir: &std::path::Path, cwd: &str, sess: &str) -> (PathBuf, PathBuf) {
    (
        working_jsonl_path(profile_dir, cwd, sess),
        working_sidecar_dir(profile_dir, cwd, sess),
    )
}

fn line_count(p: &std::path::Path) -> usize {
    fs::read_to_string(p)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────────
// The test
// ──────────────────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_first_turn_swap_back_swap_crash_recovery_finalize() {
    let tmp = tempfile::TempDir::new().unwrap();
    let anatta_home = tmp.path();

    let conv_id = ulid::Ulid::new().to_string();
    let cwd_str = "/Users/test/code";

    // Profile A = anthropic = a-native (strict)
    let profile_a_dir = anatta_home.join("profiles/claude-AAA");
    fs::create_dir_all(profile_a_dir.join("projects")).unwrap();
    let family_a = family_of(BackendKind::Claude, "anthropic", None);
    assert_eq!(family_a, Family::ANative);

    // Profile B = deepseek = a-compat (lax)
    let profile_b_dir = anatta_home.join("profiles/claude-BBB");
    fs::create_dir_all(profile_b_dir.join("projects")).unwrap();
    let family_b = family_of(BackendKind::Claude, "deepseek", None);
    assert_eq!(family_b, Family::ACompat);

    // ── 1. Segment 0 opens under profile A. session_uuid not yet known. ──
    let seg0_id = ulid::Ulid::new().to_string();
    let mut seg0 = Segment {
        id: seg0_id.clone(),
        profile_dir: profile_a_dir.clone(),
        central_dir: central_dir(anatta_home, &conv_id, &seg0_id),
        source_family: family_a,
        last_absorbed_bytes: 0,
        render_initial_bytes: 0,
        ended: false,
    };

    // First-turn render must be a no-op (session_uuid is unknown).
    let outcome = render_into_working(
        &[],
        family_a,
        &profile_a_dir.join("projects/dummy.jsonl"),
        &profile_a_dir.join("projects/dummy"),
        /* session_uuid_is_present */ false,
    )
    .unwrap();
    assert_eq!(outcome, RenderOutcome::SkippedFirstTurn);

    // ── 2. Simulate claude generating a UUID and writing the first turn. ──
    let sess_uuid = "11111111-2222-3333-4444-555555555555";
    let (work_a, work_a_sidecar) = working_paths(&profile_a_dir, cwd_str, sess_uuid);
    let first_turn = vec![
        user_msg("u1", None, "hello", sess_uuid),
        assistant_thinking(
            "a1",
            "u1",
            "thinking-strict",
            "REAL_ANTHROPIC_SIG",
            sess_uuid,
        ),
        assistant_text("a2", "a1", "hi back", sess_uuid),
    ];
    append_lines(&work_a, &first_turn);

    // ── 3. Absorb segment 0's first turn. ──
    let abs = absorb_after_turn(AbsorbInput {
        working_jsonl: &work_a,
        working_sidecar: &work_a_sidecar,
        central_events: &seg0.central_events(),
        central_sidecar: &seg0.central_sidecar(),
        last_absorbed_bytes: seg0.last_absorbed_bytes,
        render_initial_bytes: seg0.render_initial_bytes,
    })
    .unwrap();
    match abs {
        AbsorbOutcome::Absorbed {
            appended_bytes,
            new_last_absorbed,
        } => {
            assert!(appended_bytes > 0, "first turn should produce bytes");
            seg0.last_absorbed_bytes = new_last_absorbed;
        }
        other => panic!("expected Absorbed, got {other:?}"),
    }
    assert_eq!(line_count(&seg0.central_events()), 3, "3 events absorbed");
    let central_body = fs::read_to_string(seg0.central_events()).unwrap();
    assert!(central_body.contains("REAL_ANTHROPIC_SIG"));

    // ── 4. Second turn under profile A. ──
    let second_turn = vec![
        user_msg("u2", Some("a2"), "follow up", sess_uuid),
        assistant_text("a3", "u2", "second response", sess_uuid),
    ];
    append_lines(&work_a, &second_turn);
    let abs = absorb_after_turn(AbsorbInput {
        working_jsonl: &work_a,
        working_sidecar: &work_a_sidecar,
        central_events: &seg0.central_events(),
        central_sidecar: &seg0.central_sidecar(),
        last_absorbed_bytes: seg0.last_absorbed_bytes,
        render_initial_bytes: seg0.render_initial_bytes,
    })
    .unwrap();
    if let AbsorbOutcome::Absorbed {
        new_last_absorbed, ..
    } = abs
    {
        seg0.last_absorbed_bytes = new_last_absorbed;
    } else {
        panic!("expected Absorbed");
    }
    assert_eq!(
        line_count(&seg0.central_events()),
        5,
        "5 events after 2nd turn"
    );

    // ── 5. Profile swap A → B (same-family target lax, so Verbatim). ──
    seg0.ended = true;
    let seg1_id = ulid::Ulid::new().to_string();
    let policy = min_policy_for(seg0.source_family, family_b);
    assert_eq!(
        policy,
        SegmentRenderPolicy::Verbatim,
        "a-native→a-compat is Verbatim"
    );

    // Render into profile B's projects, under the SAME session_uuid.
    let (work_b, work_b_sidecar) = working_paths(&profile_b_dir, cwd_str, sess_uuid);
    let outcome = render_into_working(
        &[PriorSegmentInput {
            segment_id: seg0.id.clone(),
            source_family: seg0.source_family,
            central_events_path: seg0.central_events(),
            central_sidecar_dir: seg0.central_sidecar(),
        }],
        family_b,
        &work_b,
        &work_b_sidecar,
        true,
    )
    .unwrap();
    let initial_b = match outcome {
        RenderOutcome::Rendered { working_bytes } => working_bytes,
        _ => panic!("expected Rendered"),
    };
    // Working file under B should match central seg 0 byte-for-byte
    let body_b = fs::read_to_string(&work_b).unwrap();
    let body_central = fs::read_to_string(seg0.central_events()).unwrap();
    assert_eq!(body_b, body_central, "Verbatim copy under profile B");

    let mut seg1 = Segment {
        id: seg1_id.clone(),
        profile_dir: profile_b_dir.clone(),
        central_dir: central_dir(anatta_home, &conv_id, &seg1_id),
        source_family: family_b,
        last_absorbed_bytes: initial_b,
        render_initial_bytes: initial_b,
        ended: false,
    };

    // ── 6. Turn under profile B with a (fake-signed) thinking block. ──
    let b_turn = vec![
        user_msg("u3", Some("a3"), "deep think please", sess_uuid),
        assistant_thinking("a4", "u3", "thinking-lax", "FAKE_PROXY_SIG", sess_uuid),
        assistant_text("a5", "a4", "lax response", sess_uuid),
    ];
    append_lines(&work_b, &b_turn);
    let abs = absorb_after_turn(AbsorbInput {
        working_jsonl: &work_b,
        working_sidecar: &work_b_sidecar,
        central_events: &seg1.central_events(),
        central_sidecar: &seg1.central_sidecar(),
        last_absorbed_bytes: seg1.last_absorbed_bytes,
        render_initial_bytes: seg1.render_initial_bytes,
    })
    .unwrap();
    if let AbsorbOutcome::Absorbed {
        new_last_absorbed, ..
    } = abs
    {
        seg1.last_absorbed_bytes = new_last_absorbed;
    }
    assert_eq!(
        line_count(&seg1.central_events()),
        3,
        "seg 1's central holds ONLY the 3 new events"
    );
    let seg1_body = fs::read_to_string(seg1.central_events()).unwrap();
    assert!(
        seg1_body.contains("FAKE_PROXY_SIG"),
        "seg 1 retains its (bogus) signature"
    );

    // ── 7. Crash-recovery test: simulate offset DB update failure on seg 1. ──
    // Re-run absorb with the SAME old offset — should detect already-applied.
    let absorb_again = absorb_after_turn(AbsorbInput {
        working_jsonl: &work_b,
        working_sidecar: &work_b_sidecar,
        central_events: &seg1.central_events(),
        central_sidecar: &seg1.central_sidecar(),
        last_absorbed_bytes: initial_b, // pretend we never advanced
        render_initial_bytes: seg1.render_initial_bytes,
    })
    .unwrap();
    match absorb_again {
        AbsorbOutcome::Absorbed {
            appended_bytes,
            new_last_absorbed,
        } => {
            assert_eq!(appended_bytes, 0, "crash recovery: no duplicate append");
            assert_eq!(new_last_absorbed, seg1.last_absorbed_bytes);
        }
        other => panic!("expected Absorbed (recovery), got {other:?}"),
    }
    // Central seg 1 still has exactly 3 events (no duplication)
    assert_eq!(line_count(&seg1.central_events()), 3);

    // ── 8. Swap back B → A (lax → strict, requires StripReasoning for seg 1). ──
    seg1.ended = true;
    let seg2_id = ulid::Ulid::new().to_string();

    let policy_for_seg0 = min_policy_for(seg0.source_family, family_a);
    let policy_for_seg1 = min_policy_for(seg1.source_family, family_a);
    assert_eq!(policy_for_seg0, SegmentRenderPolicy::Verbatim);
    assert_eq!(policy_for_seg1, SegmentRenderPolicy::StripReasoning);

    let (work_a_v2, work_a_v2_sidecar) = working_paths(&profile_a_dir, cwd_str, sess_uuid);
    // Delete the old working file under A (simulating cleanup on the prior swap-out)
    let _ = fs::remove_file(&work_a_v2);

    let outcome = render_into_working(
        &[
            PriorSegmentInput {
                segment_id: seg0.id.clone(),
                source_family: seg0.source_family,
                central_events_path: seg0.central_events(),
                central_sidecar_dir: seg0.central_sidecar(),
            },
            PriorSegmentInput {
                segment_id: seg1.id.clone(),
                source_family: seg1.source_family,
                central_events_path: seg1.central_events(),
                central_sidecar_dir: seg1.central_sidecar(),
            },
        ],
        family_a,
        &work_a_v2,
        &work_a_v2_sidecar,
        true,
    )
    .unwrap();
    let initial_a2 = match outcome {
        RenderOutcome::Rendered { working_bytes } => working_bytes,
        _ => panic!("expected Rendered"),
    };

    let rendered_body = fs::read_to_string(&work_a_v2).unwrap();
    // Seg 0's content is present verbatim
    assert!(rendered_body.contains("REAL_ANTHROPIC_SIG"));
    assert!(rendered_body.contains("hi back"));
    // Seg 1's bogus signature is GONE (sanitizer stripped it)
    assert!(
        !rendered_body.contains("FAKE_PROXY_SIG"),
        "StripReasoning must remove the bogus signature"
    );
    // Seg 1's user msg + final text are still there (only thinking is dropped)
    assert!(rendered_body.contains("deep think please"));
    assert!(rendered_body.contains("lax response"));
    // The thinking-only assistant event a4 is gone; a5's parentUuid should
    // have been relinked from "a4" → "u3".
    let a5_line = rendered_body
        .lines()
        .find(|l| l.contains("\"a5\""))
        .expect("a5 still in output");
    let a5_value: serde_json::Value = serde_json::from_str(a5_line).unwrap();
    assert_eq!(
        a5_value.get("parentUuid").and_then(|v| v.as_str()),
        Some("u3"),
        "a5 relinked over the dropped thinking event"
    );

    // ── 9. Final turn under seg 2 (profile A). ──
    let mut seg2 = Segment {
        id: seg2_id.clone(),
        profile_dir: profile_a_dir.clone(),
        central_dir: central_dir(anatta_home, &conv_id, &seg2_id),
        source_family: family_a,
        last_absorbed_bytes: initial_a2,
        render_initial_bytes: initial_a2,
        ended: false,
    };
    let final_turn = vec![
        user_msg("u4", Some("a5"), "all good?", sess_uuid),
        assistant_text("a6", "u4", "yes thanks", sess_uuid),
    ];
    append_lines(&work_a_v2, &final_turn);
    let abs = absorb_after_turn(AbsorbInput {
        working_jsonl: &work_a_v2,
        working_sidecar: &work_a_v2_sidecar,
        central_events: &seg2.central_events(),
        central_sidecar: &seg2.central_sidecar(),
        last_absorbed_bytes: seg2.last_absorbed_bytes,
        render_initial_bytes: seg2.render_initial_bytes,
    })
    .unwrap();
    if let AbsorbOutcome::Absorbed {
        new_last_absorbed, ..
    } = abs
    {
        seg2.last_absorbed_bytes = new_last_absorbed;
    }
    assert_eq!(
        line_count(&seg2.central_events()),
        2,
        "seg 2 central holds only its 2 new events"
    );

    // ── 10. Finalize session: delete working area; central is preserved. ──
    let _ = fs::remove_file(&work_a_v2);
    let _ = fs::remove_dir_all(&work_a_v2_sidecar);
    assert!(!work_a_v2.exists());
    // Central still intact
    assert_eq!(line_count(&seg0.central_events()), 5);
    assert_eq!(line_count(&seg1.central_events()), 3);
    assert_eq!(line_count(&seg2.central_events()), 2);

    // ── 11. Verify the conversation directory layout matches the spec. ──
    let conv_dir = anatta_home.join("conversations").join(&conv_id);
    assert!(
        conv_dir
            .join("segments")
            .join(&seg0.id)
            .join("events.jsonl")
            .exists()
    );
    assert!(
        conv_dir
            .join("segments")
            .join(&seg1.id)
            .join("events.jsonl")
            .exists()
    );
    assert!(
        conv_dir
            .join("segments")
            .join(&seg2.id)
            .join("events.jsonl")
            .exists()
    );

    // ── 12. Verify path encoding matches what claude expects. ──
    assert_eq!(encode_cwd("/Users/test/code"), "-Users-test-code");

    eprintln!("✓ full lifecycle test passed:");
    eprintln!(
        "  seg 0 (a-native): {} events in central",
        line_count(&seg0.central_events())
    );
    eprintln!(
        "  seg 1 (a-compat): {} events in central",
        line_count(&seg1.central_events())
    );
    eprintln!(
        "  seg 2 (a-native): {} events in central",
        line_count(&seg2.central_events())
    );
}

/// Regression: a legacy conversation (one that existed before tier 1)
/// has its JSONL at `<profile>/projects/<encoded_cwd>/<session>.jsonl`
/// but central storage has no content for it yet. A render with empty
/// prior (because central is empty) must NOT silently truncate the
/// existing working file. Either:
///   (a) Migration has run and central has the content → render reproduces it
///   (b) Migration hasn't run → render refuses with WouldEmptyOverwrite
///
/// This test exercises scenario (b): the safety net catches the data-loss case.
#[test]
fn legacy_working_file_protected_from_empty_render() {
    let tmp = tempfile::TempDir::new().unwrap();
    let profile_dir = tmp.path().join("profiles/claude-AAA");
    fs::create_dir_all(profile_dir.join("projects")).unwrap();
    let cwd = "/Users/test/legacy";
    let session = "deadbeef-1111-2222-3333-444444444444";

    // Pre-existing legacy JSONL with content at the working path.
    let (work, work_sidecar) = working_paths(&profile_dir, cwd, session);
    let legacy_content = "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"sessionId\":\"deadbeef\",\"message\":{\"role\":\"user\",\"content\":\"hello from before tier 1\"}}\n";
    append_lines(&work, &[legacy_content.trim_end().to_string()]);
    let pre_size = fs::metadata(&work).unwrap().len();
    assert!(pre_size > 0);

    // Central storage is empty (no segments materialized yet).
    // Render with no prior segments — this is the bug scenario.
    let outcome = render_into_working(&[], Family::ANative, &work, &work_sidecar, true);
    match outcome {
        Err(anatta_runtime::conversation::RenderError::WouldEmptyOverwrite {
            existing_bytes,
            ..
        }) => {
            assert_eq!(existing_bytes, pre_size, "safety net protects legacy bytes");
        }
        other => panic!("expected WouldEmptyOverwrite, got {other:?}"),
    }

    // Working file untouched
    assert_eq!(fs::metadata(&work).unwrap().len(), pre_size);
    assert!(
        fs::read_to_string(&work)
            .unwrap()
            .contains("hello from before tier 1"),
        "legacy content preserved"
    );
}

/// Regression: after legacy migration has populated central, render
/// must reproduce the same content (verbatim for same-family). The
/// working file gets re-written with content matching what was there
/// before, but no data is lost.
#[test]
fn legacy_migration_then_render_preserves_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let anatta_home = tmp.path();
    let profile_dir = anatta_home.join("profiles/claude-AAA");
    fs::create_dir_all(profile_dir.join("projects")).unwrap();
    let cwd = "/Users/test/legacy";
    let session = "deadbeef-1111-2222-3333-444444444444";

    // Step 1: pre-existing legacy JSONL.
    let (work, work_sidecar) = working_paths(&profile_dir, cwd, session);
    let lines = vec![
        user_msg("u1", None, "first message", session),
        assistant_text("a1", "u1", "first response", session),
    ];
    append_lines(&work, &lines);
    let pre_size = fs::metadata(&work).unwrap().len();

    // Step 2: simulate what `ensure_active_segment + migrate_legacy_jsonl_if_needed`
    // do — copy the legacy file into central seg 0 events.jsonl.
    let conv_id = ulid::Ulid::new().to_string();
    let seg_id = ulid::Ulid::new().to_string();
    let central_events = central_dir(anatta_home, &conv_id, &seg_id).join("events.jsonl");
    fs::create_dir_all(central_events.parent().unwrap()).unwrap();
    fs::copy(&work, &central_events).unwrap();

    // Step 3: render with the migrated segment as active.
    // After migration, last_absorbed_bytes = render_initial_bytes = pre_size.
    let outcome = render_into_working(
        &[PriorSegmentInput {
            segment_id: seg_id.clone(),
            source_family: Family::ANative,
            central_events_path: central_events.clone(),
            central_sidecar_dir: tmp.path().join("nonexistent-sidecar"),
        }],
        Family::ANative,
        &work,
        &work_sidecar,
        true,
    )
    .unwrap();
    let working_bytes = match outcome {
        RenderOutcome::Rendered { working_bytes } => working_bytes,
        other => panic!("expected Rendered, got {other:?}"),
    };
    assert_eq!(
        working_bytes, pre_size,
        "post-migration render reproduces same byte count",
    );
    let post = fs::read_to_string(&work).unwrap();
    assert!(post.contains("first message"));
    assert!(post.contains("first response"));
}

#[test]
fn first_turn_no_render_then_absorb() {
    let tmp = tempfile::TempDir::new().unwrap();
    let anatta_home = tmp.path();
    let profile_dir = anatta_home.join("profiles/claude-XYZ");
    fs::create_dir_all(profile_dir.join("projects")).unwrap();

    let conv_id = ulid::Ulid::new().to_string();
    let seg_id = ulid::Ulid::new().to_string();
    let central = central_dir(anatta_home, &conv_id, &seg_id);
    let cwd_str = "/private/tmp/anatta-e2e";

    // session_uuid not yet known → render is SkippedFirstTurn
    let outcome = render_into_working(
        &[],
        Family::ANative,
        &profile_dir.join("projects/whatever.jsonl"),
        &profile_dir.join("projects/whatever"),
        false,
    )
    .unwrap();
    assert_eq!(outcome, RenderOutcome::SkippedFirstTurn);

    // CLI generates UUID and writes first turn
    let sess = "abcdef00-1111-2222-3333-444444444444";
    let (work, work_sidecar) = working_paths(&profile_dir, cwd_str, sess);
    let lines = vec![
        user_msg("u1", None, "hi", sess),
        assistant_text("a1", "u1", "ok", sess),
    ];
    append_lines(&work, &lines);

    let abs = absorb_after_turn(AbsorbInput {
        working_jsonl: &work,
        working_sidecar: &work_sidecar,
        central_events: &central.join("events.jsonl"),
        central_sidecar: &central.join("sidecar"),
        last_absorbed_bytes: 0,
        render_initial_bytes: 0,
    })
    .unwrap();
    match abs {
        AbsorbOutcome::Absorbed { appended_bytes, .. } => assert!(appended_bytes > 0),
        other => panic!("expected Absorbed, got {other:?}"),
    }
    assert_eq!(line_count(&central.join("events.jsonl")), 2);
}
