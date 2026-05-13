//! Cache discipline + on-demand transcode for per-segment views.
//!
//! Per the tier 3 design (canonical-in-source-engine + lazy
//! per-target view), render asks this module: "for segment X whose
//! source engine is S, give me a path to its content in target
//! engine T's wire format". The module:
//!
//!   * Returns the canonical path directly when S == T.
//!   * Returns a cached view path under `<segment_dir>/views/<T>/`
//!     when one exists and is current.
//!   * Otherwise transcodes into a fresh atomic `<view_dir>.tmp` and
//!     renames; returns the new view path.
//!
//! `_meta.json` inside the view dir records the transcoder version
//! and the source canonical size at build time; either differing
//! from the live state means rebuild.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{transcode_to, Engine, TranscodeError, TranscodeInput, TRANSCODER_VERSION};

/// Result of a per-segment cache decision.
#[derive(Debug)]
pub struct CacheLookup {
    /// Path to the segment's events file in the target engine's
    /// native wire shape (canonical or transcoded view).
    pub events_path: PathBuf,
    /// Path to the sidecar dir corresponding to that view (claude-only;
    /// codex sub-agents live as separate rollouts and this dir is
    /// expected to be empty on the codex side).
    pub sidecar_dir: PathBuf,
    /// Whether the result came from a freshly-rebuilt view (true) or
    /// an existing canonical / cache hit (false). For telemetry /
    /// logging only.
    pub rebuilt: bool,
}

/// Where one segment's canonical + view data lives in central.
///
/// Caller supplies these paths because the central store layout is
/// owned by the orchestration crate.
#[derive(Debug, Clone)]
pub struct SegmentLocation<'a> {
    pub source_engine: Engine,
    pub central_events_path: &'a Path,
    pub central_sidecar_dir: &'a Path,
    /// Root of the segment's `views/` subdirectory:
    /// `<anatta_home>/conversations/<conv>/segments/<sid>/views/`.
    pub views_root: &'a Path,
    /// Used to derive the deterministic view session id when transcoding.
    pub source_engine_session_id: &'a str,
}

/// Resolve the right path to feed render's per-segment concatenation
/// for a given target engine. May trigger an on-demand transcode +
/// cache population.
pub fn resolve_for_target(
    target: Engine,
    seg: SegmentLocation<'_>,
    conversation_cwd: &str,
) -> Result<CacheLookup, TranscodeError> {
    if target == seg.source_engine {
        return Ok(CacheLookup {
            events_path: seg.central_events_path.to_path_buf(),
            sidecar_dir: seg.central_sidecar_dir.to_path_buf(),
            rebuilt: false,
        });
    }

    let view_dir = seg.views_root.join(target.as_str());
    if view_is_current(&view_dir, seg.central_events_path)? {
        return Ok(CacheLookup {
            events_path: view_dir.join(target_events_filename(target)),
            sidecar_dir: view_dir.join("sidecar"),
            rebuilt: false,
        });
    }

    // Stale or missing — rebuild.
    transcode_to(
        target,
        TranscodeInput {
            source_engine: seg.source_engine,
            source_events_jsonl: seg.central_events_path,
            source_sidecar_dir: seg.central_sidecar_dir,
            source_engine_session_id: seg.source_engine_session_id,
            conversation_cwd,
        },
        &view_dir,
    )?;

    // Write _meta.json after the atomic-rename so its presence implies
    // the rest of the view dir is consistent.
    let canonical_size = fs::metadata(seg.central_events_path)
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);
    let meta = ViewMeta {
        transcoder_version: TRANSCODER_VERSION,
        source_canonical_size_at_build: canonical_size,
    };
    let meta_path = view_dir.join("_meta.json");
    let tmp_meta = with_tmp_suffix(&meta_path);
    {
        let mut f = fs::File::create(&tmp_meta)?;
        let buf = serde_json::to_vec_pretty(&meta).map_err(|e| TranscodeError::Parse {
            line: 0,
            source: e,
        })?;
        f.write_all(&buf)?;
        f.flush()?;
    }
    fs::rename(&tmp_meta, &meta_path)?;

    Ok(CacheLookup {
        events_path: view_dir.join(target_events_filename(target)),
        sidecar_dir: view_dir.join("sidecar"),
        rebuilt: true,
    })
}

fn target_events_filename(target: Engine) -> &'static str {
    match target {
        Engine::Claude => "events.jsonl",
        Engine::Codex => "rollout.jsonl",
    }
}

fn view_is_current(view_dir: &Path, canonical_events_path: &Path) -> Result<bool, TranscodeError> {
    let meta_path = view_dir.join("_meta.json");
    if !meta_path.exists() {
        return Ok(false);
    }
    let raw = fs::read(&meta_path)?;
    let meta: ViewMeta = match serde_json::from_slice(&raw) {
        Ok(m) => m,
        Err(_) => return Ok(false), // malformed _meta — treat as stale
    };
    if meta.transcoder_version != TRANSCODER_VERSION {
        return Ok(false);
    }
    let canonical_size = match fs::metadata(canonical_events_path) {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if meta.source_canonical_size_at_build != canonical_size {
        return Ok(false);
    }
    // Also verify the produced view file actually exists. (Defensive
    // against partial cleanup post-rename.)
    let claude_path = view_dir.join("events.jsonl");
    let codex_path = view_dir.join("rollout.jsonl");
    Ok(claude_path.exists() || codex_path.exists())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewMeta {
    transcoder_version: u32,
    source_canonical_size_at_build: u64,
}

fn with_tmp_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_minimal_claude_jsonl(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            r#"{"type":"user","sessionId":"src","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"#,
        )
        .unwrap();
    }

    #[test]
    fn same_engine_returns_canonical_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("events.jsonl");
        let sidecar = tmp.path().join("sidecar");
        fs::write(&canonical, "{}\n").unwrap();
        let views_root = tmp.path().join("views");

        let r = resolve_for_target(
            Engine::Claude,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();

        assert_eq!(r.events_path, canonical);
        assert!(!r.rebuilt);
    }

    #[test]
    fn cross_engine_first_call_rebuilds_then_second_call_is_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("events.jsonl");
        let sidecar = tmp.path().join("sidecar");
        write_minimal_claude_jsonl(&canonical);
        let views_root = tmp.path().join("views");

        let r1 = resolve_for_target(
            Engine::Codex,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();
        assert!(r1.rebuilt, "first cross-engine call should transcode");
        assert!(r1.events_path.ends_with("rollout.jsonl"));

        let r2 = resolve_for_target(
            Engine::Codex,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();
        assert!(!r2.rebuilt, "second call should hit cache");
        assert_eq!(r1.events_path, r2.events_path);
    }

    #[test]
    fn canonical_growth_invalidates_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("events.jsonl");
        let sidecar = tmp.path().join("sidecar");
        write_minimal_claude_jsonl(&canonical);
        let views_root = tmp.path().join("views");

        // First call — builds.
        let r1 = resolve_for_target(
            Engine::Codex,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();
        assert!(r1.rebuilt);

        // Mutate canonical — append a turn.
        let mut data = fs::read_to_string(&canonical).unwrap();
        data.push_str(
            r#"{"type":"assistant","sessionId":"src","message":{"role":"assistant","content":[{"type":"text","text":"hi back"}]}}
"#,
        );
        fs::write(&canonical, data).unwrap();

        let r2 = resolve_for_target(
            Engine::Codex,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();
        assert!(r2.rebuilt, "canonical size change should invalidate cache");
    }

    #[test]
    fn cross_engine_creates_view_meta_with_correct_version() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("events.jsonl");
        let sidecar = tmp.path().join("sidecar");
        write_minimal_claude_jsonl(&canonical);
        let views_root = tmp.path().join("views");

        resolve_for_target(
            Engine::Codex,
            SegmentLocation {
                source_engine: Engine::Claude,
                central_events_path: &canonical,
                central_sidecar_dir: &sidecar,
                views_root: &views_root,
                source_engine_session_id: "src",
            },
            "/work",
        )
        .unwrap();

        let meta_path = views_root.join("codex").join("_meta.json");
        assert!(meta_path.exists());
        let m: ViewMeta = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        assert_eq!(m.transcoder_version, TRANSCODER_VERSION);
        assert_eq!(
            m.source_canonical_size_at_build,
            fs::metadata(&canonical).unwrap().len()
        );
    }
}
