//! Tier-1 conversation orchestration: render / absorb / segment lifecycle.
//!
//! This module is the bridge between:
//!   - the SQL store (conversations + conversation_segments tables)
//!   - the runtime's pure file IO (render / absorb / sanitize)
//!   - the spawn layer (session lifecycle)
//!
//! Used by `chat::runner` (multi-turn) and `send` (one-shot resume).

use std::path::PathBuf;

use anatta_runtime::conversation::render_v2::{PriorSegmentV2, RenderV2Error, render_v2};
use anatta_runtime::conversation::{
    AbsorbError, AbsorbInput, AbsorbOutcome, RenderError, RenderOutcome, absorb_after_turn,
};
use anatta_runtime::conversation::{working_jsonl_path, working_sidecar_dir};
use anatta_runtime::profile::{BackendKind, Family, family_of, min_policy_for};
use anatta_runtime::transcode::Engine;
use anatta_store::conversation::ConversationMetadata;
use anatta_store::profile::ProfileRecord;
use anatta_store::segment::SegmentRecord;

use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum OrchError {
    #[error("store: {0}")]
    Store(#[from] anatta_store::StoreError),
    #[error("render: {0}")]
    Render(#[from] RenderError),
    #[error("render-v2: {0}")]
    RenderV2(#[from] RenderV2Error),
    #[error("absorb: {0}")]
    Absorb(#[from] AbsorbError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("conversation not found: {0}")]
    NotFound(String),
    #[allow(dead_code)] // reserved for callers that need to disambiguate
    #[error("conversation {0} has no active segment")]
    NoActiveSegment(String),
    #[error("unsupported segment backend: {0}")]
    UnknownBackend(String),
}

fn engine_of_backend(backend: &str) -> Result<Engine, OrchError> {
    Engine::parse(backend).ok_or_else(|| OrchError::UnknownBackend(backend.to_owned()))
}

/// Engine-aware path to the working main file.
///
/// claude: `<profile>/projects/<encoded_cwd>/<engine_session_id>.jsonl`
/// codex:  `<profile>/sessions/anatta/rollout-<engine_session_id>.jsonl`
///
/// The codex path uses a flat `sessions/anatta/` subdir rather than
/// the YYYY/MM/DD scheme codex itself uses, so anatta-managed
/// rollouts are easy to locate and clean up. codex auto-registers the
/// `threads` row from `session_meta` on first `thread/resume`, so the
/// path itself doesn't matter to codex.
pub fn engine_working_main_path(
    profile_dir: &std::path::Path,
    conv_cwd: &str,
    engine_session_id: &str,
    backend: Engine,
) -> PathBuf {
    match backend {
        Engine::Claude => working_jsonl_path(profile_dir, conv_cwd, engine_session_id),
        Engine::Codex => profile_dir
            .join("sessions")
            .join("anatta")
            .join(format!("rollout-{engine_session_id}.jsonl")),
    }
}

/// Engine-aware sidecar root for the active segment's working area.
pub fn engine_working_sidecar_dir(
    profile_dir: &std::path::Path,
    conv_cwd: &str,
    engine_session_id: &str,
    backend: Engine,
) -> PathBuf {
    match backend {
        Engine::Claude => working_sidecar_dir(profile_dir, conv_cwd, engine_session_id),
        // codex sub-agents live as sibling rollout files; we don't have a single
        // "sidecar dir" the way claude does. Point at a stable empty subdir
        // so callers that mkdir it succeed harmlessly.
        Engine::Codex => profile_dir
            .join("sessions")
            .join("anatta")
            .join(format!("rollout-{engine_session_id}.sidecar")),
    }
}

/// Per-segment central `views/` root used by the transcoder cache.
pub fn segment_views_root(anatta_home: &std::path::Path, conv_id: &str, seg_id: &str) -> PathBuf {
    anatta_home
        .join("conversations")
        .join(conv_id)
        .join("segments")
        .join(seg_id)
        .join("views")
}

/// Compute the family for a profile, using its provider + override.
pub fn profile_family(p: &ProfileRecord) -> Family {
    let backend = match p.backend {
        anatta_store::profile::BackendKind::Claude => BackendKind::Claude,
        anatta_store::profile::BackendKind::Codex => BackendKind::Codex,
    };
    family_of(backend, &p.provider, p.family_override.as_deref())
}

/// Ensure the conversation has a backfilled `id` + an active segment.
/// If the conversation predates migration 0006, this populates the
/// new columns AND creates a "segment 0" row representing the legacy
/// history.
///
/// For legacy conversations whose JSONL already lives at the shared
/// `<profile>/projects/<encoded_cwd>/<session_uuid>.jsonl` (because the
/// pre-tier-1 architecture wrote sessions there), this also performs a
/// one-time content migration: the existing JSONL is copied into the new
/// central `segments/<seg_0_id>/events.jsonl` and segment offsets are
/// seeded to match. After this migration, render+absorb operate on
/// central as the canonical store.
///
/// Idempotent. Returns the active segment's record.
pub async fn ensure_active_segment(
    cfg: &Config,
    conv_name: &str,
    profile: &ProfileRecord,
) -> Result<SegmentRecord, OrchError> {
    let conv_id = cfg.store.ensure_conversation_metadata(conv_name).await?;
    if let Some(s) = cfg.store.active_segment(&conv_id).await? {
        // Active segment exists — make sure legacy content has been migrated.
        // This handles the case where ensure_active_segment ran in a prior
        // process, created the segment row, but didn't get to migration
        // (or crashed mid-flight).
        if let Ok(Some(meta)) = cfg.store.get_conversation_metadata(conv_name).await {
            migrate_legacy_jsonl_if_needed(cfg, &meta, profile, &s).await?;
        }
        return Ok(s);
    }
    // No active segment — initialize segment 0 (or next ordinal).
    let segments = cfg.store.list_segments(&conv_id).await?;
    let next_ordinal = segments.last().map(|s| s.ordinal + 1).unwrap_or(0);
    let seg_id = ulid::Ulid::new().to_string();
    let family = profile_family(profile);
    let policy_json = serde_json::to_string(&serde_json::json!({"kind":"verbatim"})).unwrap();
    cfg.store
        .insert_segment(anatta_store::segment::NewSegment {
            id: &seg_id,
            conversation_id: &conv_id,
            ordinal: next_ordinal,
            profile_id: &profile.id,
            source_family: family.as_str(),
            transition_policy: &policy_json,
            backend: profile.backend.as_str(),
            engine_session_id: None,
        })
        .await?;
    let active = cfg
        .store
        .active_segment(&conv_id)
        .await?
        .expect("just inserted");
    // After segment 0 (or first segment, in the case of a legacy backfill)
    // is in place, copy any pre-existing JSONL content from the profile's
    // working area into central. This is a one-shot migration; after this
    // point central is the source of truth.
    if let Ok(Some(meta)) = cfg.store.get_conversation_metadata(conv_name).await {
        migrate_legacy_jsonl_if_needed(cfg, &meta, profile, &active).await?;
    }
    Ok(active)
}

/// One-shot migration: copy the pre-tier-1 working JSONL (at the
/// `<profile>/projects/<encoded_cwd>/<session_uuid>.jsonl` location)
/// into the segment's central events.jsonl, and seed offsets so subsequent
/// absorbs only capture truly-new bytes.
///
/// No-op when:
///   - session_uuid is unknown (no legacy file to find)
///   - the segment isn't at ordinal 0 (legacy only lives in the first segment)
///   - central events.jsonl already has content (migration already done)
///   - the legacy file doesn't exist or is empty
async fn migrate_legacy_jsonl_if_needed(
    cfg: &Config,
    conv: &ConversationMetadata,
    profile: &ProfileRecord,
    active_segment: &SegmentRecord,
) -> Result<(), OrchError> {
    // Source: prefer segment.engine_session_id (tier 3), fall back to
    // conv.session_uuid (only populated pre-destructive-drop). After
    // the drop, the latter is always None; the migration 0007 backfill
    // ensured ordinal-0 segments inherited the value into
    // engine_session_id, so we don't lose anything.
    let session_uuid = match active_segment
        .engine_session_id
        .as_deref()
        .or(conv.session_uuid.as_deref())
    {
        Some(s) => s,
        None => return Ok(()),
    };
    let Some(conv_id) = conv.id.as_deref() else {
        return Ok(());
    };
    if active_segment.ordinal != 0 {
        return Ok(());
    }
    let central_events = segment_events_path(&cfg.anatta_home, conv_id, &active_segment.id);
    let central_has_content = std::fs::metadata(&central_events)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if central_has_content {
        return Ok(());
    }
    let profile_dir = profile.path_for_runtime(cfg)?;
    let legacy_jsonl =
        anatta_runtime::conversation::working_jsonl_path(&profile_dir, &conv.cwd, session_uuid);
    let legacy_size = match std::fs::metadata(&legacy_jsonl) {
        Ok(m) if m.len() > 0 => m.len(),
        _ => return Ok(()), // no legacy content to migrate
    };
    // Copy bytes to central.
    if let Some(parent) = central_events.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&legacy_jsonl, &central_events)?;
    // Mirror legacy sidecar dir if present.
    let legacy_sidecar =
        anatta_runtime::conversation::working_sidecar_dir(&profile_dir, &conv.cwd, session_uuid);
    if legacy_sidecar.exists() {
        let central_sidecar = segment_sidecar_dir(&cfg.anatta_home, conv_id, &active_segment.id);
        anatta_runtime::conversation::sidecar::copy_dir_recursive(
            &legacy_sidecar,
            &central_sidecar,
        )?;
    }
    // Seed offsets: working file currently has `legacy_size` bytes and
    // central matches. Both `last_absorbed_bytes` and `render_initial_bytes`
    // are equal to legacy_size at this checkpoint, which is the same state
    // a fresh `render` would produce.
    cfg.store
        .set_segment_offsets(
            &active_segment.id,
            legacy_size as i64,
            Some(legacy_size as i64),
        )
        .await?;
    // Seed engine_session_id on the (ordinal-0) segment from the legacy
    // conversations.session_uuid so subsequent tier-3 resume paths can
    // find the working file by segment id alone.
    if active_segment.engine_session_id.is_none() {
        cfg.store
            .set_engine_session_id(&active_segment.id, session_uuid)
            .await?;
    }
    eprintln!(
        "[anatta] migrated {} bytes from legacy working file → central segment {}",
        legacy_size, active_segment.id
    );
    Ok(())
}

/// Open a new segment for a profile swap. Closes the active one first.
/// Computes the transition policy as `min_policy_for(prev.source_family,
/// family_of(new_profile))`.
pub async fn open_segment_for_swap(
    cfg: &Config,
    conv_name: &str,
    new_profile: &ProfileRecord,
    ended_with_compact: bool,
) -> Result<SegmentRecord, OrchError> {
    let meta = cfg
        .store
        .get_conversation_metadata(conv_name)
        .await?
        .ok_or_else(|| OrchError::NotFound(conv_name.to_owned()))?;
    let conv_id = meta
        .id
        .clone()
        .ok_or_else(|| OrchError::NotFound(conv_name.to_owned()))?;

    let prev = cfg.store.active_segment(&conv_id).await?;
    let prev_family = prev
        .as_ref()
        .and_then(|s| Family::parse(&s.source_family))
        .unwrap_or(Family::ACompat);
    let new_family = profile_family(new_profile);

    if let Some(p) = &prev {
        cfg.store.close_segment(&p.id, ended_with_compact).await?;
    }
    let next_ordinal = prev.as_ref().map(|s| s.ordinal + 1).unwrap_or(0);
    let policy = min_policy_for(prev_family, new_family);
    let policy_json = serde_json::to_string(&policy).expect("serde");
    let seg_id = ulid::Ulid::new().to_string();
    // Pre-allocate engine_session_id for the new segment so render can
    // pre-populate the working file with transcoded prior history under
    // a stable resume coordinate. Without this, render would
    // SkippedFirstTurn and the new engine would launch a fresh thread
    // with no context — defeating cross-engine continuity.
    let minted_engine_session_id = anatta_runtime::transcode::id_mint::mint_engine_session_id();
    cfg.store
        .insert_segment(anatta_store::segment::NewSegment {
            id: &seg_id,
            conversation_id: &conv_id,
            ordinal: next_ordinal,
            profile_id: &new_profile.id,
            source_family: new_family.as_str(),
            transition_policy: &policy_json,
            backend: new_profile.backend.as_str(),
            engine_session_id: Some(&minted_engine_session_id),
        })
        .await?;
    Ok(cfg
        .store
        .active_segment(&conv_id)
        .await?
        .expect("just inserted"))
}

/// Render all prior segments' central events into the working file,
/// applying per-segment family policy + transcoder cache for any
/// foreign-engine prior segments.
///
/// Working file path is engine-aware (claude → `projects/<cwd>/<id>.jsonl`,
/// codex → `sessions/anatta/rollout-<id>.jsonl`). When the active
/// segment's `engine_session_id` is NULL (first turn), returns
/// `SkippedFirstTurn` without writing — the engine will mint the id
/// and we capture it post-turn via `set_active_segment_engine_id`.
///
/// After a successful render, the active segment's `last_absorbed_bytes`
/// and `render_initial_bytes` are both set to the rendered file size.
pub async fn render_for_session(
    cfg: &Config,
    conv: &ConversationMetadata,
    profile: &ProfileRecord,
    active_segment_id: &str,
) -> Result<RenderOutcome, OrchError> {
    let conv_id = conv
        .id
        .clone()
        .ok_or_else(|| OrchError::NotFound("conversation id".into()))?;
    let target_engine = engine_of_backend(profile.backend.as_str())?;
    let target_family = profile_family(profile);

    let active = cfg
        .store
        .get_segment(active_segment_id)
        .await?
        .ok_or_else(|| OrchError::NotFound(format!("segment {active_segment_id}")))?;
    // Engine session id for the active segment. First-turn case → None,
    // render is a no-op (engine will mint it).
    let active_engine_session_id = active.engine_session_id.clone();

    let segments = cfg.store.list_segments(&conv_id).await?;
    let mut prior: Vec<PriorSegmentV2> = Vec::new();
    for seg in &segments {
        // Include closed segments AND the active one. Skip any future
        // segments (shouldn't exist; partial unique index forbids it).
        if seg.id != active_segment_id && seg.ended_at.is_none() {
            continue;
        }
        let source_engine = engine_of_backend(seg.backend.as_str())?;
        let source_family = Family::parse(&seg.source_family).unwrap_or(Family::ACompat);
        let source_engine_session_id = seg.engine_session_id.clone().unwrap_or_default();
        prior.push(PriorSegmentV2 {
            segment_id: seg.id.clone(),
            source_engine,
            source_family,
            source_engine_session_id,
            central_events_path: segment_events_path(&cfg.anatta_home, &conv_id, &seg.id),
            central_sidecar_dir: segment_sidecar_dir(&cfg.anatta_home, &conv_id, &seg.id),
            views_root: segment_views_root(&cfg.anatta_home, &conv_id, &seg.id),
        });
    }

    let profile_dir = profile.path_for_runtime(cfg)?;
    let working_main = engine_working_main_path(
        &profile_dir,
        &conv.cwd,
        active_engine_session_id.as_deref().unwrap_or(""),
        target_engine,
    );
    let working_sidecar = engine_working_sidecar_dir(
        &profile_dir,
        &conv.cwd,
        active_engine_session_id.as_deref().unwrap_or(""),
        target_engine,
    );

    let outcome = render_v2(
        &prior,
        target_engine,
        target_family,
        active_engine_session_id.as_deref(),
        &conv.cwd,
        &working_main,
        &working_sidecar,
    )?;

    if let RenderOutcome::Rendered { working_bytes } = outcome {
        cfg.store
            .set_segment_offsets(
                active_segment_id,
                working_bytes as i64,
                Some(working_bytes as i64),
            )
            .await?;
    }
    Ok(outcome)
}

/// Persist an engine-generated session/thread id onto the active
/// segment. Idempotent — only sets when currently NULL on the segment.
///
/// During tier 3 transition (while `conversations.session_uuid` still
/// exists in the schema), this ALSO dual-writes to that legacy column
/// for backward compat with code paths that haven't been ported yet.
/// Once `enable_destructive_drop` retires the legacy column, the
/// dual-write becomes a single write — the call to `set_session_uuid`
/// will fail at SQL level if the column is gone, but by then no
/// caller should be reading from it.
pub async fn set_active_segment_engine_id_if_needed(
    cfg: &Config,
    conv_name: &str,
    active_segment: &SegmentRecord,
    engine_session_id: &str,
) -> Result<(), OrchError> {
    if active_segment.engine_session_id.is_some() {
        return Ok(());
    }
    cfg.store
        .set_engine_session_id(&active_segment.id, engine_session_id)
        .await?;
    // Best-effort dual-write to the legacy column. If the column has
    // been dropped (post tier 3 destructive migration), the SQL fails;
    // we swallow it because the segment-side write is the new source
    // of truth.
    let _ = cfg
        .store
        .set_session_uuid(conv_name, engine_session_id)
        .await;
    Ok(())
}

/// Absorb new bytes from the working file into the active segment's
/// central events.jsonl. Updates `last_absorbed_bytes` on success.
///
/// Tier 3: working file path is engine-aware (claude → projects/, codex →
/// sessions/anatta/), and we read the engine session id from the
/// active segment row (not `conv.session_uuid`).
pub async fn absorb_after_turn_for_session(
    cfg: &Config,
    conv: &ConversationMetadata,
    profile: &ProfileRecord,
    active_segment: &SegmentRecord,
) -> Result<AbsorbOutcome, OrchError> {
    let Some(engine_session_id) = active_segment.engine_session_id.as_deref() else {
        return Ok(AbsorbOutcome::NoWorkingFile);
    };
    let conv_id = conv
        .id
        .as_deref()
        .ok_or_else(|| OrchError::NotFound("conversation id".into()))?;

    let target_engine = engine_of_backend(active_segment.backend.as_str())?;
    let profile_dir = profile.path_for_runtime(cfg)?;
    let working_jsonl =
        engine_working_main_path(&profile_dir, &conv.cwd, engine_session_id, target_engine);
    let working_sidecar =
        engine_working_sidecar_dir(&profile_dir, &conv.cwd, engine_session_id, target_engine);
    let central_events = segment_events_path(&cfg.anatta_home, conv_id, &active_segment.id);
    let central_sidecar = segment_sidecar_dir(&cfg.anatta_home, conv_id, &active_segment.id);

    let outcome = absorb_after_turn(AbsorbInput {
        working_jsonl: &working_jsonl,
        working_sidecar: &working_sidecar,
        central_events: &central_events,
        central_sidecar: &central_sidecar,
        last_absorbed_bytes: active_segment.last_absorbed_bytes as u64,
        render_initial_bytes: active_segment.render_initial_bytes as u64,
    })?;

    if let AbsorbOutcome::Absorbed {
        new_last_absorbed, ..
    } = outcome
    {
        cfg.store
            .set_segment_offsets(&active_segment.id, new_last_absorbed as i64, None)
            .await?;
    }

    // For codex segments, also harvest sub-agent rollouts (if any)
    // into the central sidecar. The state DB is read-only so this is
    // safe to run concurrently with codex. Errors here are non-fatal
    // (main absorb already succeeded).
    if target_engine == Engine::Codex {
        if let Err(e) = absorb_codex_sub_agents(cfg, conv_id, active_segment, &profile_dir).await {
            eprintln!("[anatta] codex sub-agent absorb skipped: {e}");
        }
    }

    Ok(outcome)
}

async fn absorb_codex_sub_agents(
    cfg: &Config,
    conv_id: &str,
    active_segment: &SegmentRecord,
    codex_profile_dir: &std::path::Path,
) -> Result<(), OrchError> {
    let Some(thread_id) = active_segment.engine_session_id.as_deref() else {
        return Ok(());
    };
    let state_db = codex_profile_dir.join("state_5.sqlite");
    let subs = anatta_store::codex_state::list_sub_agent_rollouts(&state_db, thread_id).await?;
    if subs.is_empty() {
        return Ok(());
    }
    let dest_sidecar =
        segment_sidecar_dir(&cfg.anatta_home, conv_id, &active_segment.id).join("subagents");
    std::fs::create_dir_all(&dest_sidecar)?;
    for s in subs {
        let dst = dest_sidecar.join(format!("{}.jsonl", s.child_thread_id));
        if dst.exists() {
            continue; // already mirrored
        }
        if let Err(e) = std::fs::copy(&s.rollout_path, &dst) {
            eprintln!(
                "[anatta] failed to mirror codex sub-rollout {}: {e}",
                s.rollout_path
            );
        }
    }
    Ok(())
}

/// At session end: final absorb, delete working file + sidecar, reset
/// segment offsets (so the next session re-renders).
pub async fn finalize_session(
    cfg: &Config,
    conv: &ConversationMetadata,
    profile: &ProfileRecord,
    active_segment: &SegmentRecord,
) -> Result<(), OrchError> {
    // Final absorb (also harvests codex sub-agents).
    let _ = absorb_after_turn_for_session(cfg, conv, profile, active_segment).await?;

    if let Some(engine_session_id) = active_segment.engine_session_id.as_deref() {
        let target_engine = engine_of_backend(active_segment.backend.as_str())?;
        let profile_dir = profile.path_for_runtime(cfg)?;
        let working_jsonl =
            engine_working_main_path(&profile_dir, &conv.cwd, engine_session_id, target_engine);
        let working_sidecar =
            engine_working_sidecar_dir(&profile_dir, &conv.cwd, engine_session_id, target_engine);
        let _ = std::fs::remove_file(&working_jsonl);
        let _ = std::fs::remove_dir_all(&working_sidecar);
    }

    cfg.store.reset_segment_offsets(&active_segment.id).await?;
    Ok(())
}

/// Compute the path to a segment's central events.jsonl.
pub fn segment_events_path(anatta_home: &std::path::Path, conv_id: &str, seg_id: &str) -> PathBuf {
    anatta_home
        .join("conversations")
        .join(conv_id)
        .join("segments")
        .join(seg_id)
        .join("events.jsonl")
}

/// Compute the path to a segment's central sidecar dir.
pub fn segment_sidecar_dir(anatta_home: &std::path::Path, conv_id: &str, seg_id: &str) -> PathBuf {
    anatta_home
        .join("conversations")
        .join(conv_id)
        .join("segments")
        .join(seg_id)
        .join("sidecar")
}

/// Profile-dir path helper that converts the store's profile id back to
/// the runtime ClaudeProfile / CodexProfile path. We can't store the
/// path directly on `ProfileRecord` without altering its shape; this
/// helper does the lookup on demand.
pub trait ProfilePathExt {
    fn path_for_runtime(&self, cfg: &Config) -> Result<PathBuf, OrchError>;
}

impl ProfilePathExt for ProfileRecord {
    fn path_for_runtime(&self, cfg: &Config) -> Result<PathBuf, OrchError> {
        Ok(cfg.anatta_home.join("profiles").join(&self.id))
    }
}
