//! Render: central segments → working JSONL file.
//!
//! Called at session start (and on profile swap). For each prior segment,
//! choose a policy by `min_policy_for(seg.source_family, target_family)`
//! and apply it while concatenating into the working file. Sidecars are
//! mirrored verbatim (no policy applied at the sidecar level in tier 1).
//!
//! Returns the final byte size of the rendered working file; the caller
//! uses this to seed `last_absorbed_bytes` / `render_initial_bytes` for
//! the active segment.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::claude::sanitize::{SanitizeError, strip_reasoning};
use crate::profile::{Family, SegmentRenderPolicy, min_policy_for};

use super::sidecar::copy_dir_recursive;

/// One prior segment's identity + provenance, as supplied to render.
///
/// `central_events_path` may not exist if the first turn never produced
/// any content (e.g., an aborted session); render skips it gracefully.
#[derive(Debug, Clone)]
pub struct PriorSegmentInput {
    pub segment_id: String,
    pub source_family: Family,
    pub central_events_path: PathBuf,
    pub central_sidecar_dir: PathBuf,
}

/// Outcome of a render call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderOutcome {
    /// Conversation has no session_uuid yet (first turn pending);
    /// no working file was produced. Caller should spawn claude
    /// without --session-id / --resume.
    SkippedFirstTurn,
    /// Working file written; this is the byte count to use as both
    /// `last_absorbed_bytes` and `render_initial_bytes` for the active
    /// segment.
    Rendered { working_bytes: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("policy not implemented in tier 1: {0:?}")]
    PolicyNotImplemented(SegmentRenderPolicy),
    #[error(
        "refusing to overwrite non-empty working file with empty render output at {path}: \
         {existing_bytes} bytes would be lost. This usually means central storage is empty \
         but the working area already has content (legacy migration pending or central was wiped)."
    )]
    WouldEmptyOverwrite { path: PathBuf, existing_bytes: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sanitize(#[from] SanitizeError),
}

/// Render `prior_segments` (in given order) into a fresh working file at
/// `working_jsonl`, applying per-segment policy.
///
/// If `session_uuid_is_present` is `false`, this is a first-turn call:
/// no render is performed (CLI will generate the file). Returns
/// `RenderOutcome::SkippedFirstTurn` in that case.
///
/// Atomicity: writes to `<working_jsonl>.tmp` then renames. Sidecar
/// directories are copied into `<working_sidecar>.tmp` and renamed.
/// If the final rename of either succeeds but the other fails, the
/// caller must surface a clear error and consider the working state
/// inconsistent (callers should not proceed with spawn in that case).
pub fn render_into_working(
    prior_segments: &[PriorSegmentInput],
    target_family: Family,
    working_jsonl: &Path,
    working_sidecar: &Path,
    session_uuid_is_present: bool,
) -> Result<RenderOutcome, RenderError> {
    if !session_uuid_is_present {
        return Ok(RenderOutcome::SkippedFirstTurn);
    }

    let parent = working_jsonl
        .parent()
        .expect("working path must have a parent dir");
    fs::create_dir_all(parent)?;

    let tmp_main = with_tmp_suffix(working_jsonl, "jsonl.tmp");
    {
        let mut out = BufWriter::new(File::create(&tmp_main)?);
        for seg in prior_segments {
            let policy = min_policy_for(seg.source_family, target_family);
            if !seg.central_events_path.exists() {
                // First-turn-pending or zero-event segment — skip
                continue;
            }
            let src_file = File::open(&seg.central_events_path)?;
            let reader = BufReader::new(src_file);
            match policy {
                SegmentRenderPolicy::Verbatim => {
                    let mut r = reader;
                    std::io::copy(&mut r, &mut out)?;
                }
                SegmentRenderPolicy::StripReasoning => {
                    strip_reasoning(reader, &mut out)?;
                }
                other => return Err(RenderError::PolicyNotImplemented(other)),
            }
        }
        out.flush()?;
    }

    // Sidecar: copy all prior segments' sidecar contents into a single
    // tmp directory, then atomically rename.
    let tmp_sidecar = working_sidecar.with_extension("sidecar.tmp");
    let mut sidecar_used = false;
    let sidecar_copy_result: Result<(), std::io::Error> = (|| {
        for seg in prior_segments {
            if seg.central_sidecar_dir.exists() {
                copy_dir_recursive(&seg.central_sidecar_dir, &tmp_sidecar)?;
                sidecar_used = true;
            }
        }
        Ok(())
    })();
    match sidecar_copy_result {
        Ok(()) => {
            if sidecar_used {
                if working_sidecar.exists() {
                    fs::remove_dir_all(working_sidecar)?;
                }
                fs::rename(&tmp_sidecar, working_sidecar)?;
            }
        }
        Err(e) => {
            // Tear down tmp sidecar; do NOT proceed with main rename
            // (don't leave the user with main updated but sidecar partial).
            let _ = fs::remove_dir_all(&tmp_sidecar);
            let _ = fs::remove_file(&tmp_main);
            return Err(e.into());
        }
    }

    // SAFETY NET: refuse to overwrite a non-empty target with empty output.
    // If render produced 0 bytes but the working file already has content,
    // someone is in a state where central is empty but the working area
    // isn't (legacy migration not yet applied, central store wiped, etc.).
    // We must not clobber the working file.
    let tmp_size = fs::metadata(&tmp_main)?.len();
    if tmp_size == 0 {
        if let Ok(existing) = fs::metadata(working_jsonl) {
            if existing.len() > 0 {
                let _ = fs::remove_file(&tmp_main);
                return Err(RenderError::WouldEmptyOverwrite {
                    path: working_jsonl.to_owned(),
                    existing_bytes: existing.len(),
                });
            }
        }
    }

    // Commit main file last (sidecar succeeded above).
    fs::rename(&tmp_main, working_jsonl)?;

    let bytes = fs::metadata(working_jsonl)?.len();
    Ok(RenderOutcome::Rendered {
        working_bytes: bytes,
    })
}

fn with_tmp_suffix(p: &Path, ext: &str) -> PathBuf {
    p.with_extension(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = File::create(path).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn refuses_to_empty_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");
        // Pre-existing legacy content at the target path.
        fs::create_dir_all(working.parent().unwrap()).unwrap();
        fs::write(&working, b"legacy turn 1 content\nlegacy turn 2 content\n").unwrap();
        let prev_bytes = fs::metadata(&working).unwrap().len();

        // Now ask render to produce empty output (no prior segments).
        let err = render_into_working(&[], Family::ANative, &working, &sidecar, true).unwrap_err();
        match err {
            RenderError::WouldEmptyOverwrite {
                path,
                existing_bytes,
            } => {
                assert_eq!(path, working);
                assert_eq!(existing_bytes, prev_bytes);
            }
            other => panic!("expected WouldEmptyOverwrite, got {other:?}"),
        }
        // Original file untouched.
        assert_eq!(fs::metadata(&working).unwrap().len(), prev_bytes);
        assert!(
            fs::read_to_string(&working)
                .unwrap()
                .contains("legacy turn 1"),
            "original content preserved",
        );
    }

    #[test]
    fn first_turn_skip_returns_correct_outcome() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");
        let outcome = render_into_working(
            &[],
            Family::ANative,
            &working,
            &sidecar,
            /* session_uuid_is_present */ false,
        )
        .unwrap();
        assert_eq!(outcome, RenderOutcome::SkippedFirstTurn);
        assert!(!working.exists(), "should not create working file");
    }

    #[test]
    fn verbatim_concatenates_segments_in_order() {
        let tmp = TempDir::new().unwrap();
        let seg0 = tmp.path().join("seg0.jsonl");
        let seg1 = tmp.path().join("seg1.jsonl");
        write_jsonl(
            &seg0,
            &[
                r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"S","message":{"role":"user","content":"hi"}}"#,
            ],
        );
        write_jsonl(
            &seg1,
            &[
                r#"{"type":"user","uuid":"u2","parentUuid":"u1","sessionId":"S","message":{"role":"user","content":"again"}}"#,
            ],
        );
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");

        let prior = vec![
            PriorSegmentInput {
                segment_id: "seg0".into(),
                source_family: Family::ANative,
                central_events_path: seg0,
                central_sidecar_dir: tmp.path().join("seg0-sidecar"),
            },
            PriorSegmentInput {
                segment_id: "seg1".into(),
                source_family: Family::ANative,
                central_events_path: seg1,
                central_sidecar_dir: tmp.path().join("seg1-sidecar"),
            },
        ];

        let outcome = render_into_working(
            &prior,
            Family::ANative,
            &working,
            &sidecar,
            /* session_uuid_is_present */ true,
        )
        .unwrap();

        let RenderOutcome::Rendered { working_bytes } = outcome else {
            panic!("expected Rendered, got {outcome:?}");
        };
        assert!(working_bytes > 0);
        let body = fs::read_to_string(&working).unwrap();
        assert!(body.contains("\"u1\""));
        assert!(body.contains("\"u2\""));
        let pos_u1 = body.find("\"u1\"").unwrap();
        let pos_u2 = body.find("\"u2\"").unwrap();
        assert!(pos_u1 < pos_u2, "segments concatenated in order");
    }

    #[test]
    fn lax_to_strict_strips_thinking() {
        let tmp = TempDir::new().unwrap();
        let seg0 = tmp.path().join("seg0.jsonl");
        // a-compat segment with one user event + one thinking + one text
        write_jsonl(
            &seg0,
            &[
                r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"S","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"S","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"FAKE_SIG"}]}}"#,
                r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":"S","message":{"role":"assistant","content":[{"type":"text","text":"hello back"}]}}"#,
            ],
        );
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");

        let prior = vec![PriorSegmentInput {
            segment_id: "seg0".into(),
            source_family: Family::ACompat,
            central_events_path: seg0,
            central_sidecar_dir: tmp.path().join("seg0-sidecar"),
        }];

        render_into_working(
            &prior,
            Family::ANative,
            &working,
            &sidecar,
            /* session_uuid_is_present */ true,
        )
        .unwrap();

        let body = fs::read_to_string(&working).unwrap();
        assert!(!body.contains("FAKE_SIG"), "thinking signature stripped");
        assert!(!body.contains("\"a1\""), "thinking event dropped");
        assert!(body.contains("\"u1\""));
        assert!(body.contains("\"a2\""));
        // a2's parent should now point to u1 (relinked)
        let a2_line = body.lines().find(|l| l.contains("\"a2\"")).unwrap();
        let v: serde_json::Value = serde_json::from_str(a2_line).unwrap();
        assert_eq!(v["parentUuid"].as_str(), Some("u1"));
    }

    #[test]
    fn strict_to_lax_is_verbatim() {
        let tmp = TempDir::new().unwrap();
        let seg0 = tmp.path().join("seg0.jsonl");
        write_jsonl(
            &seg0,
            &[
                r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"S","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"REAL_SIG"}]}}"#,
                r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":"S","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
            ],
        );
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");

        let prior = vec![PriorSegmentInput {
            segment_id: "seg0".into(),
            source_family: Family::ANative,
            central_events_path: seg0,
            central_sidecar_dir: tmp.path().join("seg0-sidecar"),
        }];

        render_into_working(&prior, Family::ACompat, &working, &sidecar, true).unwrap();

        let body = fs::read_to_string(&working).unwrap();
        assert!(
            body.contains("REAL_SIG"),
            "signature preserved on strict→lax"
        );
        assert!(body.contains("\"a1\""), "thinking event kept verbatim");
    }

    #[test]
    fn empty_segments_produce_empty_file() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");
        let outcome = render_into_working(&[], Family::ANative, &working, &sidecar, true).unwrap();
        assert!(matches!(
            outcome,
            RenderOutcome::Rendered { working_bytes: 0 }
        ));
        assert!(working.exists());
        assert_eq!(fs::metadata(&working).unwrap().len(), 0);
    }

    #[test]
    fn missing_segment_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");
        // central path that doesn't exist
        let prior = vec![PriorSegmentInput {
            segment_id: "seg0".into(),
            source_family: Family::ANative,
            central_events_path: tmp.path().join("does-not-exist.jsonl"),
            central_sidecar_dir: tmp.path().join("does-not-exist-sidecar"),
        }];

        let outcome =
            render_into_working(&prior, Family::ANative, &working, &sidecar, true).unwrap();
        assert!(matches!(
            outcome,
            RenderOutcome::Rendered { working_bytes: 0 }
        ));
    }

    #[test]
    fn sidecar_mirrored_into_working() {
        let tmp = TempDir::new().unwrap();
        let seg0_sidecar = tmp.path().join("seg0-sidecar");
        fs::create_dir_all(seg0_sidecar.join("subagents")).unwrap();
        fs::write(seg0_sidecar.join("subagents/agent-1.jsonl"), b"sub").unwrap();

        let seg0_events = tmp.path().join("seg0.jsonl");
        write_jsonl(&seg0_events, &[]);

        let working = tmp.path().join("working.jsonl");
        let sidecar = tmp.path().join("sidecar");

        let prior = vec![PriorSegmentInput {
            segment_id: "seg0".into(),
            source_family: Family::ANative,
            central_events_path: seg0_events,
            central_sidecar_dir: seg0_sidecar,
        }];

        render_into_working(&prior, Family::ANative, &working, &sidecar, true).unwrap();

        assert!(sidecar.join("subagents/agent-1.jsonl").exists());
    }
}
