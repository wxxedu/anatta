//! Absorb: working JSONL → central segment events.jsonl (offset-based).
//!
//! After each CLI exit, anatta reads the working JSONL from its stored
//! `last_absorbed_bytes` to current EOF, appends those bytes to the
//! active segment's central events.jsonl, and updates the offset.
//!
//! Crash-idempotent: if anatta crashes between appending and updating
//! the offset, the next absorb run uses `render_initial_bytes` to detect
//! and skip a duplicate append. The invariant we rely on is that the
//! central file's size after a fully-applied absorb equals
//! `last_absorbed_bytes - render_initial_bytes`. On entry we check the
//! actual central size; if it already matches the post-append target,
//! we treat the append as already-done and just update the offset.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::sidecar::sync_sidecar_one_way;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsorbOutcome {
    /// The working file does not exist (e.g., still pre-first-turn).
    /// Caller treats as a no-op.
    NoWorkingFile,
    /// Working file existed; this many new bytes were appended to central.
    /// `new_last_absorbed` is the new offset to persist.
    Absorbed {
        appended_bytes: u64,
        new_last_absorbed: u64,
    },
    /// Working file existed but was at or behind `last_absorbed_bytes`.
    /// Treated as a no-op.
    Nothing,
}

#[derive(Debug, thiserror::Error)]
pub enum AbsorbError {
    #[error("working file shrunk: previous offset {previous}, current size {current}")]
    WorkingFileShrunk { previous: u64, current: u64 },
    #[error(
        "central events file unexpectedly larger than expected ({actual} > {expected}) — possible duplicate or external write"
    )]
    CentralOversize { actual: u64, expected: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Inputs needed to perform an absorb after a turn.
#[derive(Debug, Clone)]
pub struct AbsorbInput<'a> {
    /// Working file (in profile dir).
    pub working_jsonl: &'a Path,
    /// Working sidecar dir (in profile dir).
    pub working_sidecar: &'a Path,
    /// Central events.jsonl for the active segment.
    pub central_events: &'a Path,
    /// Central sidecar dir for the active segment.
    pub central_sidecar: &'a Path,
    /// Persisted `last_absorbed_bytes` from before this turn.
    pub last_absorbed_bytes: u64,
    /// Persisted `render_initial_bytes` for the active segment. Used to
    /// compute the expected central file size for crash-idempotency.
    pub render_initial_bytes: u64,
}

/// Perform an absorb. Returns the new `last_absorbed_bytes` to persist.
pub fn absorb_after_turn(input: AbsorbInput<'_>) -> Result<AbsorbOutcome, AbsorbError> {
    if !input.working_jsonl.exists() {
        return Ok(AbsorbOutcome::NoWorkingFile);
    }
    let cur_size = fs::metadata(input.working_jsonl)?.len();
    if cur_size < input.last_absorbed_bytes {
        return Err(AbsorbError::WorkingFileShrunk {
            previous: input.last_absorbed_bytes,
            current: cur_size,
        });
    }
    if cur_size == input.last_absorbed_bytes {
        // Even if the JSONL didn't grow, sidecar files might have. Sync them.
        if input.working_sidecar.exists() {
            sync_sidecar_one_way(input.working_sidecar, input.central_sidecar)?;
        }
        return Ok(AbsorbOutcome::Nothing);
    }

    // Crash-recovery invariant check.
    // Expected central size after a fully-applied absorb = last_absorbed_bytes - render_initial_bytes.
    let expected_central_after_prior = input
        .last_absorbed_bytes
        .saturating_sub(input.render_initial_bytes);
    let actual_central = if input.central_events.exists() {
        fs::metadata(input.central_events)?.len()
    } else {
        0
    };

    if actual_central > expected_central_after_prior {
        // Could mean: a previous absorb's bytes were written to central
        // but the offset wasn't updated (anatta crashed between rename
        // and DB commit). Detect by comparing: if actual_central matches
        // the size we WOULD reach after this absorb, treat this as
        // already-done.
        let new_total_if_applied =
            expected_central_after_prior + (cur_size - input.last_absorbed_bytes);
        if actual_central == new_total_if_applied {
            // Bytes were already appended in a prior crashed run.
            // Just sync the sidecar and update the offset.
            if input.working_sidecar.exists() {
                sync_sidecar_one_way(input.working_sidecar, input.central_sidecar)?;
            }
            return Ok(AbsorbOutcome::Absorbed {
                appended_bytes: 0,
                new_last_absorbed: cur_size,
            });
        }
        // Resume-after-render case: a prior session populated central
        // with absorbed claude content, then chat closed and resumed.
        // On resume, render rebuilt working from central and bumped
        // both `last_absorbed_bytes` and `render_initial_bytes` to the
        // rendered working size — but did not truncate central. Central
        // still holds the prior absorbed content (the same data render
        // re-projected into working). The new claude bytes beyond
        // `last_absorbed_bytes` in working can be safely appended on
        // top. Recognize this state by checking that central is aligned
        // to `last_absorbed_bytes`; if so, fall through to the normal
        // append path below.
        if actual_central != input.last_absorbed_bytes {
            return Err(AbsorbError::CentralOversize {
                actual: actual_central,
                expected: expected_central_after_prior,
            });
        }
        // Fall through to normal append.
    }

    // Normal path: read [last_absorbed_bytes .. cur_size) from working,
    // append to central via a tmp-rename pattern for atomicity.
    let n = (cur_size - input.last_absorbed_bytes) as usize;
    let mut src = File::open(input.working_jsonl)?;
    src.seek(SeekFrom::Start(input.last_absorbed_bytes))?;
    let mut buf = vec![0u8; n];
    src.read_exact(&mut buf)?;

    let parent = input
        .central_events
        .parent()
        .expect("central_events must have a parent");
    fs::create_dir_all(parent)?;

    let tmp_central = input.central_events.with_extension("jsonl.tmp");
    if input.central_events.exists() {
        fs::copy(input.central_events, &tmp_central)?;
    }
    {
        let mut dst = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp_central)?;
        dst.write_all(&buf)?;
        dst.flush()?;
    }
    fs::rename(&tmp_central, input.central_events)?;

    // Sidecar mirror — append-only semantics.
    if input.working_sidecar.exists() {
        sync_sidecar_one_way(input.working_sidecar, input.central_sidecar)?;
    }

    Ok(AbsorbOutcome::Absorbed {
        appended_bytes: n as u64,
        new_last_absorbed: cur_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn no_working_file_is_no_op() {
        let tmp = TempDir::new().unwrap();
        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &tmp.path().join("nonexistent.jsonl"),
            working_sidecar: &tmp.path().join("nonexistent-sidecar"),
            central_events: &tmp.path().join("central.jsonl"),
            central_sidecar: &tmp.path().join("central-sidecar"),
            last_absorbed_bytes: 0,
            render_initial_bytes: 0,
        })
        .unwrap();
        assert_eq!(outcome, AbsorbOutcome::NoWorkingFile);
    }

    #[test]
    fn first_turn_absorb_full_file() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");
        let content = b"line1\nline2\nline3\n";
        write(&working, content);

        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 0,
            render_initial_bytes: 0,
        })
        .unwrap();

        match outcome {
            AbsorbOutcome::Absorbed {
                appended_bytes,
                new_last_absorbed,
            } => {
                assert_eq!(appended_bytes, content.len() as u64);
                assert_eq!(new_last_absorbed, content.len() as u64);
            }
            other => panic!("expected Absorbed, got {other:?}"),
        }
        assert_eq!(fs::read(&central).unwrap(), content);
    }

    #[test]
    fn incremental_absorb_after_render() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");

        // Simulate: render put 10 bytes into working; offset is at 10.
        write(&working, b"RENDEREDXX");
        // Central has nothing yet (segment is newly opened).

        // Turn produces 6 more bytes.
        write(&working, b"RENDEREDXXturn1\n");

        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 10,
            render_initial_bytes: 10,
        })
        .unwrap();

        match outcome {
            AbsorbOutcome::Absorbed { appended_bytes, .. } => {
                assert_eq!(appended_bytes, 6);
            }
            other => panic!("expected Absorbed, got {other:?}"),
        }
        assert_eq!(fs::read(&central).unwrap(), b"turn1\n");
    }

    #[test]
    fn shrunk_working_file_errors() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");
        write(&working, b"short");
        let err = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 100,
            render_initial_bytes: 0,
        })
        .unwrap_err();
        assert!(matches!(err, AbsorbError::WorkingFileShrunk { .. }));
    }

    #[test]
    fn nothing_when_offset_equals_size() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");
        write(&working, b"hello");
        write(&central, b"");
        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 5,
            render_initial_bytes: 0,
        })
        .unwrap();
        assert_eq!(outcome, AbsorbOutcome::Nothing);
    }

    #[test]
    fn crash_recovery_detects_already_appended() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");

        // Simulate: render put 0 bytes; turn produced 10 bytes; absorb
        // appended 10 bytes to central but crashed before updating offset.
        write(&working, b"0123456789");
        write(&central, b"0123456789"); // central already has the bytes

        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 0,
            render_initial_bytes: 0,
        })
        .unwrap();

        match outcome {
            AbsorbOutcome::Absorbed {
                appended_bytes,
                new_last_absorbed,
            } => {
                assert_eq!(appended_bytes, 0, "no new bytes appended this time");
                assert_eq!(new_last_absorbed, 10, "offset caught up");
            }
            other => panic!("expected Absorbed (recovered), got {other:?}"),
        }
        // central still 10 bytes, not 20
        assert_eq!(fs::metadata(&central).unwrap().len(), 10);
    }

    #[test]
    fn resume_after_render_keeps_central_aligned() {
        // Models the chat-resume scenario: a prior session absorbed 50
        // bytes into central, then render-on-resume rebuilt working to
        // 50 bytes and bumped both `last_absorbed_bytes` and
        // `render_initial_bytes` to 50 without truncating central.
        // The next turn appends 7 bytes to working. Absorb should
        // recognize this state and append cleanly.
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");
        // 50 bytes of pre-render claude content in central, plus the
        // rendered prefix in working (also 50 bytes — render
        // re-projects the same data).
        let prior = vec![b'A'; 50];
        write(&central, &prior);
        // Working now has render prefix + 7 new bytes from one new turn.
        let mut working_content = prior.clone();
        working_content.extend_from_slice(b"newturn");
        write(&working, &working_content);

        let outcome = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 50,
            render_initial_bytes: 50,
        })
        .unwrap();

        match outcome {
            AbsorbOutcome::Absorbed {
                appended_bytes,
                new_last_absorbed,
            } => {
                assert_eq!(appended_bytes, 7);
                assert_eq!(new_last_absorbed, 57);
            }
            other => panic!("expected Absorbed, got {other:?}"),
        }
        assert_eq!(fs::metadata(&central).unwrap().len(), 57);
    }

    #[test]
    fn corrupted_central_oversize_errors() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let central = tmp.path().join("central.jsonl");
        write(&working, b"0123456789");
        // Central has way more than expected — corruption.
        write(&central, b"WAY_TOO_MUCH_CONTENT_HERE");
        let err = absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &tmp.path().join("ws"),
            central_events: &central,
            central_sidecar: &tmp.path().join("cs"),
            last_absorbed_bytes: 0,
            render_initial_bytes: 0,
        })
        .unwrap_err();
        assert!(matches!(err, AbsorbError::CentralOversize { .. }));
    }

    #[test]
    fn sidecar_mirrored_on_absorb() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working.jsonl");
        let working_sidecar = tmp.path().join("ws");
        let central = tmp.path().join("central.jsonl");
        let central_sidecar = tmp.path().join("cs");
        write(&working, b"hello\n");
        fs::create_dir_all(working_sidecar.join("tool-results")).unwrap();
        fs::write(working_sidecar.join("tool-results/x.txt"), b"output").unwrap();

        absorb_after_turn(AbsorbInput {
            working_jsonl: &working,
            working_sidecar: &working_sidecar,
            central_events: &central,
            central_sidecar: &central_sidecar,
            last_absorbed_bytes: 0,
            render_initial_bytes: 0,
        })
        .unwrap();

        assert!(central_sidecar.join("tool-results/x.txt").exists());
    }
}
