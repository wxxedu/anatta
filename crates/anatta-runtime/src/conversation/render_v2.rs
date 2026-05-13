//! Tier 3 render: central segments → working file, with per-segment
//! engine routing through the transcoder cache.
//!
//! Differences from tier 1/2's [`render_into_working`]:
//!
//!   * Each prior segment carries its own `backend` (claude or codex).
//!     If it matches the target engine, render reads the canonical
//!     events file directly (and applies family policy). If it
//!     doesn't, render asks
//!     [`crate::transcode::resolve_for_target`] for a target-shaped
//!     view (which transcodes lazily if missing/stale) and
//!     concatenates that.
//!
//!   * Render writes a codex `session_meta` + `turn_context` preamble
//!     to the working file when target engine is codex.
//!
//!   * Concatenation of multiple codex segments skips each segment's
//!     own `session_meta` + first `turn_context` (so the working file
//!     has exactly one preamble — the one we wrote).
//!
//!   * Concatenation of multiple claude segments rewrites the
//!     `sessionId` field on every line to the active segment's
//!     engine_session_id (claude's resume mechanism expects a single
//!     sessionId across the file).
//!
//! All atomicity guarantees of tier 1/2 are preserved (write to tmp,
//! atomic rename).

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude::sanitize::{strip_reasoning, SanitizeError};
use crate::profile::{min_policy_for, Family, SegmentRenderPolicy};
use crate::transcode::{self, Engine, TranscodeError};

use super::render::RenderOutcome;

/// One prior segment's identity + central paths + provenance.
#[derive(Debug, Clone)]
pub struct PriorSegmentV2 {
    pub segment_id: String,
    pub source_engine: Engine,
    pub source_family: Family,
    pub source_engine_session_id: String,
    pub central_events_path: PathBuf,
    pub central_sidecar_dir: PathBuf,
    /// Root for transcoded view caches:
    /// `<anatta_home>/conversations/<conv>/segments/<sid>/views/`
    pub views_root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderV2Error {
    #[error("policy not implemented in tier 1: {0:?}")]
    PolicyNotImplemented(SegmentRenderPolicy),
    #[error(
        "refusing to overwrite non-empty working file with empty render output at {path}: \
         {existing_bytes} bytes would be lost"
    )]
    WouldEmptyOverwrite { path: PathBuf, existing_bytes: u64 },
    #[error("malformed source line during render: {line}")]
    ParseSource {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sanitize(#[from] SanitizeError),
    #[error(transparent)]
    Transcode(#[from] TranscodeError),
}

/// Render prior segments into a target-engine working file.
///
/// `target_engine_session_id` is required for codex (used in the
/// preamble's `session_meta.id`) and for claude (used to rewrite
/// per-line `sessionId` fields). For first-turn calls — when the
/// active segment hasn't yet produced an engine_session_id —
/// pass `None`; the function returns [`RenderOutcome::SkippedFirstTurn`]
/// without writing anything.
pub fn render_v2(
    prior_segments: &[PriorSegmentV2],
    target_engine: Engine,
    target_family: Family,
    target_engine_session_id: Option<&str>,
    conversation_cwd: &str,
    working_main: &Path,
    working_sidecar: &Path,
) -> Result<RenderOutcome, RenderV2Error> {
    let Some(target_session_id) = target_engine_session_id else {
        return Ok(RenderOutcome::SkippedFirstTurn);
    };

    let parent = working_main
        .parent()
        .expect("working path must have a parent dir");
    fs::create_dir_all(parent)?;

    let tmp_main = with_tmp_extension(working_main, "tmp");
    {
        let mut out = BufWriter::new(File::create(&tmp_main)?);
        if target_engine == Engine::Codex {
            write_codex_preamble(&mut out, target_session_id, conversation_cwd)?;
        }

        for seg in prior_segments {
            // Ask the cache for the right input path. Same-engine → canonical;
            // cross-engine → transcoded view (possibly built on demand).
            let lookup = transcode::resolve_for_target(
                target_engine,
                transcode::SegmentLocation {
                    source_engine: seg.source_engine,
                    central_events_path: &seg.central_events_path,
                    central_sidecar_dir: &seg.central_sidecar_dir,
                    views_root: &seg.views_root,
                    source_engine_session_id: &seg.source_engine_session_id,
                },
                conversation_cwd,
            )?;
            if !lookup.events_path.exists() {
                continue;
            }

            // Family policy applies only to same-engine source. Cross-engine
            // already dropped reasoning during transcode, so verbatim concat
            // is the right choice in that case.
            let policy = if seg.source_engine == target_engine {
                min_policy_for(seg.source_family, target_family)
            } else {
                SegmentRenderPolicy::Verbatim
            };

            apply_to_target(
                target_engine,
                &lookup.events_path,
                policy,
                target_session_id,
                &mut out,
            )?;
        }
        out.flush()?;
    }

    // Sidecar mirror. For codex target we don't currently mirror
    // sidecars into the working area (sub-agent rollouts live as
    // separate files under codex's sessions tree; the orchestration
    // layer handles them post-spawn). For claude target, mirror the
    // resolved per-segment sidecars.
    if target_engine == Engine::Claude {
        let tmp_sidecar = working_sidecar.with_extension("sidecar.tmp");
        let mut sidecar_used = false;
        let sidecar_result: Result<(), std::io::Error> = (|| {
            for seg in prior_segments {
                let lookup = match transcode::resolve_for_target(
                    target_engine,
                    transcode::SegmentLocation {
                        source_engine: seg.source_engine,
                        central_events_path: &seg.central_events_path,
                        central_sidecar_dir: &seg.central_sidecar_dir,
                        views_root: &seg.views_root,
                        source_engine_session_id: &seg.source_engine_session_id,
                    },
                    conversation_cwd,
                ) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                if lookup.sidecar_dir.exists() {
                    super::sidecar::copy_dir_recursive(&lookup.sidecar_dir, &tmp_sidecar)?;
                    sidecar_used = true;
                }
            }
            Ok(())
        })();
        match sidecar_result {
            Ok(()) => {
                if sidecar_used {
                    if working_sidecar.exists() {
                        fs::remove_dir_all(working_sidecar)?;
                    }
                    fs::rename(&tmp_sidecar, working_sidecar)?;
                }
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp_sidecar);
                let _ = fs::remove_file(&tmp_main);
                return Err(e.into());
            }
        }
    }

    let tmp_size = fs::metadata(&tmp_main)?.len();
    if tmp_size == 0 {
        if let Ok(existing) = fs::metadata(working_main) {
            if existing.len() > 0 {
                let _ = fs::remove_file(&tmp_main);
                return Err(RenderV2Error::WouldEmptyOverwrite {
                    path: working_main.to_owned(),
                    existing_bytes: existing.len(),
                });
            }
        }
    }
    fs::rename(&tmp_main, working_main)?;

    let bytes = fs::metadata(working_main)?.len();
    Ok(RenderOutcome::Rendered {
        working_bytes: bytes,
    })
}

fn apply_to_target<W: Write>(
    target: Engine,
    src_path: &Path,
    policy: SegmentRenderPolicy,
    target_session_id: &str,
    out: &mut W,
) -> Result<(), RenderV2Error> {
    match target {
        Engine::Claude => apply_claude_segment(src_path, policy, target_session_id, out),
        Engine::Codex => apply_codex_segment(src_path, policy, out),
    }
}

fn apply_claude_segment<W: Write>(
    src_path: &Path,
    policy: SegmentRenderPolicy,
    target_session_id: &str,
    out: &mut W,
) -> Result<(), RenderV2Error> {
    // For StripReasoning, run strip_reasoning on the source, then
    // sessionId-rewrite the result through the same out. We accept a
    // small staged buffer for that path to keep the implementation
    // straightforward (sessionId rewriting happens after strip).
    //
    // For Verbatim, parse / re-emit per-line so we can rewrite sessionId.
    match policy {
        SegmentRenderPolicy::Verbatim => {
            let src = File::open(src_path)?;
            let reader = BufReader::new(src);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let rewritten = rewrite_session_id(&line, target_session_id)?;
                writeln!(out, "{}", rewritten)?;
            }
        }
        SegmentRenderPolicy::StripReasoning => {
            // Stage strip_reasoning output into a Vec, then sessionId-rewrite.
            let src = File::open(src_path)?;
            let mut staged: Vec<u8> = Vec::new();
            strip_reasoning(BufReader::new(src), &mut staged)?;
            let reader = BufReader::new(&staged[..]);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let rewritten = rewrite_session_id(&line, target_session_id)?;
                writeln!(out, "{}", rewritten)?;
            }
        }
        other => return Err(RenderV2Error::PolicyNotImplemented(other)),
    }
    Ok(())
}

fn apply_codex_segment<W: Write>(
    src_path: &Path,
    policy: SegmentRenderPolicy,
    out: &mut W,
) -> Result<(), RenderV2Error> {
    // Codex preamble filter: skip the first `session_meta` and the
    // first `turn_context` (each is the bootstrap pair); preserve
    // mid-session `turn_context` lines, all `response_item`s, all
    // `event_msg`s. Policy in tier 3 is only Verbatim for cross-engine
    // (already-transcoded view) or for same-engine same-family. For
    // strict-family lax→strict on codex, we don't yet implement a
    // codex StripReasoning — tier 3 v1 falls back to Verbatim for that
    // codex same-engine path too (real codex reasoning is signed by
    // anthropic OR openai; same-engine swap usually means same
    // family).
    if !matches!(policy, SegmentRenderPolicy::Verbatim) {
        return Err(RenderV2Error::PolicyNotImplemented(policy));
    }

    let src = File::open(src_path)?;
    let reader = BufReader::new(src);
    let mut first_session_meta_seen = false;
    let mut first_turn_context_seen = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| RenderV2Error::ParseSource {
            line: line.clone(),
            source: e,
        })?;
        let event_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "session_meta" if !first_session_meta_seen => {
                first_session_meta_seen = true;
                continue;
            }
            "turn_context" if !first_turn_context_seen => {
                first_turn_context_seen = true;
                continue;
            }
            _ => {
                writeln!(out, "{}", line)?;
            }
        }
    }
    Ok(())
}

fn rewrite_session_id(line: &str, new_id: &str) -> Result<String, RenderV2Error> {
    let mut v: Value = serde_json::from_str(line).map_err(|e| RenderV2Error::ParseSource {
        line: line.to_owned(),
        source: e,
    })?;
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.insert("sessionId".to_owned(), Value::String(new_id.to_owned()));
        }
    }
    Ok(serde_json::to_string(&v).expect("re-serialize Value"))
}

fn write_codex_preamble<W: Write>(
    out: &mut W,
    session_id: &str,
    cwd: &str,
) -> Result<(), RenderV2Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let meta = serde_json::json!({
        "type": "session_meta",
        "timestamp": &now,
        "payload": {
            "id": session_id,
            "cwd": cwd,
            "originator": "anatta-render",
            "cli_version": "anatta-render/v1",
            "timestamp": &now,
            "model_provider": "openai",
            "source": "exec",
        }
    });
    let turn_context = serde_json::json!({
        "type": "turn_context",
        "timestamp": &now,
        "payload": {
            "cwd": cwd,
            "model": "",
            "approval_policy": "never",
            "sandbox_policy": { "type": "danger_full_access" },
        }
    });
    writeln!(out, "{}", meta)?;
    writeln!(out, "{}", turn_context)?;
    Ok(())
}

fn with_tmp_extension(p: &Path, ext: &str) -> PathBuf {
    p.with_extension(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mk_seg(
        tmp: &Path,
        seg_id: &str,
        engine: Engine,
        family: Family,
        engine_session_id: &str,
        canonical_content: &str,
    ) -> PriorSegmentV2 {
        let seg_dir = tmp.join("segments").join(seg_id);
        fs::create_dir_all(seg_dir.join("sidecar")).unwrap();
        fs::create_dir_all(seg_dir.join("views")).unwrap();
        let canonical = seg_dir.join("events.jsonl");
        fs::write(&canonical, canonical_content).unwrap();
        PriorSegmentV2 {
            segment_id: seg_id.to_owned(),
            source_engine: engine,
            source_family: family,
            source_engine_session_id: engine_session_id.to_owned(),
            central_events_path: canonical,
            central_sidecar_dir: seg_dir.join("sidecar"),
            views_root: seg_dir.join("views"),
        }
    }

    #[test]
    fn first_turn_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let working = tmp.path().join("work.jsonl");
        let sidecar = tmp.path().join("sidecar");
        let outcome = render_v2(
            &[],
            Engine::Claude,
            Family::ANative,
            None,
            "/cwd",
            &working,
            &sidecar,
        )
        .unwrap();
        assert!(matches!(outcome, RenderOutcome::SkippedFirstTurn));
    }

    #[test]
    fn same_engine_claude_segments_rewrite_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let seg = mk_seg(
            tmp.path(),
            "s1",
            Engine::Claude,
            Family::ANative,
            "old-session",
            r#"{"type":"user","sessionId":"old-session","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"#,
        );
        let working = tmp.path().join("work.jsonl");
        let sidecar = tmp.path().join("sidecar");
        render_v2(
            &[seg],
            Engine::Claude,
            Family::ANative,
            Some("active-session-XYZ"),
            "/cwd",
            &working,
            &sidecar,
        )
        .unwrap();
        let body = fs::read_to_string(&working).unwrap();
        assert!(body.contains("active-session-XYZ"));
        assert!(!body.contains("old-session"));
    }

    #[test]
    fn cross_engine_claude_to_codex_writes_preamble_then_transcoded() {
        let tmp = tempfile::tempdir().unwrap();
        let seg = mk_seg(
            tmp.path(),
            "s1",
            Engine::Claude,
            Family::ANative,
            "src-cc",
            r#"{"type":"user","sessionId":"src-cc","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"assistant","sessionId":"src-cc","message":{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"SIG_A"},{"type":"text","text":"hi"}]}}
"#,
        );
        let working = tmp.path().join("rollout.jsonl");
        let sidecar = tmp.path().join("sidecar");
        render_v2(
            &[seg],
            Engine::Codex,
            Family::ONative,
            Some("019c-codex-target"),
            "/work",
            &working,
            &sidecar,
        )
        .unwrap();
        let body = fs::read_to_string(&working).unwrap();
        // Preamble (target's session_meta + turn_context)
        assert!(body.contains("019c-codex-target"));
        // Transcoded content (text preserved, thinking dropped)
        assert!(body.contains("hello"));
        assert!(body.contains("hi"));
        assert!(!body.contains("SIG_A"));
        assert!(!body.contains("\"thinking\""));
        // No duplicate session_meta from a transcoded-view preamble.
        let count = body.matches("\"type\":\"session_meta\"").count();
        assert_eq!(count, 1, "exactly one session_meta in working file");
    }

    #[test]
    fn cross_engine_codex_to_claude_emits_system_init() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"src-cx","cwd":"/work","originator":"codex_exec","cli_version":"0.125.0","source":"exec","model_provider":"openai"}}"#,
            "\n",
            r#"{"type":"turn_context","timestamp":"t","payload":{"cwd":"/work","model":"gpt-5","approval_policy":"never","sandbox_policy":{"type":"danger_full_access"}}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
            "\n",
        );
        let seg = mk_seg(
            tmp.path(),
            "s1",
            Engine::Codex,
            Family::ONative,
            "src-cx",
            codex_content,
        );
        let working = tmp.path().join("events.jsonl");
        let sidecar = tmp.path().join("sidecar");
        render_v2(
            &[seg],
            Engine::Claude,
            Family::ANative,
            Some("target-claude-session"),
            "/work",
            &working,
            &sidecar,
        )
        .unwrap();
        let body = fs::read_to_string(&working).unwrap();
        // sessionId rewritten to target.
        assert!(body.contains("target-claude-session"));
        // First line should be claude system/init (from transcoder's preamble).
        let first_line = body.lines().next().unwrap();
        assert!(first_line.contains("\"type\":\"system\""));
        assert!(first_line.contains("\"subtype\":\"init\""));
    }

    #[test]
    fn mixed_segments_concat_in_order_with_one_preamble() {
        let tmp = tempfile::tempdir().unwrap();
        let s0 = mk_seg(
            tmp.path(),
            "s0",
            Engine::Claude,
            Family::ANative,
            "cc0",
            r#"{"type":"user","sessionId":"cc0","message":{"role":"user","content":[{"type":"text","text":"seg0-user"}]}}
"#,
        );
        let codex_content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"cx1","cwd":"/w","originator":"codex_exec","cli_version":"0.125.0","source":"exec","model_provider":"openai"}}"#,
            "\n",
            r#"{"type":"turn_context","timestamp":"t","payload":{"cwd":"/w","model":"gpt-5","approval_policy":"never","sandbox_policy":{"type":"danger_full_access"}}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"seg1-user"}]}}"#,
            "\n",
        );
        let s1 = mk_seg(tmp.path(), "s1", Engine::Codex, Family::ONative, "cx1", codex_content);
        let working = tmp.path().join("out.jsonl");
        let sidecar = tmp.path().join("sidecar");
        render_v2(
            &[s0, s1],
            Engine::Codex,
            Family::ONative,
            Some("active-codex"),
            "/w",
            &working,
            &sidecar,
        )
        .unwrap();
        let body = fs::read_to_string(&working).unwrap();
        // Exactly one session_meta (target's preamble).
        let count = body.matches("\"type\":\"session_meta\"").count();
        assert_eq!(count, 1);
        // Content from both segments present, in order.
        let p0 = body.find("seg0-user").expect("seg0 present");
        let p1 = body.find("seg1-user").expect("seg1 present");
        assert!(p0 < p1);
    }
}
