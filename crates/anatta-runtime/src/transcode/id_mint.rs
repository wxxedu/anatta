//! Deterministic synthetic id generation for transcoded views.
//!
//! Every id minted here is a pure function of its inputs: same inputs →
//! same id, byte for byte, across runs and machines. This matters for:
//!
//! 1. Cache stability — `view_engine_session_id` is the filename
//!    suffix of the working-area rollout codex resumes against; a
//!    drifting id would break resume between cache rebuilds.
//! 2. Sub-agent linkage — a parent view emits a `new_thread_id` that
//!    the corresponding sub-view writes as its `session_meta.id`; both
//!    sides compute the id identically.
//! 3. Tool call pairing — a function call and its matching output
//!    carry the same mapped id; the mapper is deterministic.
//!
//! v1 implementation uses readable namespaced string ids
//! (`anatta-view-<source>-claude`) rather than UUID v5 to avoid
//! adding a workspace dependency. Tier 4 may switch to UUID v5 if the
//! downstream engines reject non-UUID-shaped ids; the current spike
//! shows codex accepts arbitrary ASCII for `session_meta.id` so this
//! is fine for v1.

use super::Engine;

/// Synthetic session/thread id for a view of one segment under one
/// target engine. Deterministic.
pub fn view_session_id(source_engine_session_id: &str, target: Engine) -> String {
    format!(
        "anatta-view-{}-{}",
        sanitize_for_id(source_engine_session_id),
        target.as_str()
    )
}

/// Synthetic sub-agent thread id, derived from the parent view and the
/// sub-agent's positional index in the parent segment.
pub fn view_sub_thread_id(parent_view_id: &str, sub_agent_index: usize) -> String {
    format!("{}-sub-{}", parent_view_id, sub_agent_index)
}

/// Map a tool call id across engines. The pairing of (call, result)
/// uses the same id on both ends, so mapping a single id consistently
/// preserves pairing.
pub fn map_tool_call_id(source_id: &str, source: Engine, target: Engine) -> String {
    let prefix = match (source, target) {
        (Engine::Claude, Engine::Codex) => "anatta-cc-",
        (Engine::Codex, Engine::Claude) => "anatta-cx-",
        _ => return source_id.to_owned(),
    };
    format!("{}{}", prefix, source_id)
}

/// Mint a deterministic synthetic UUID-like value used for claude's
/// per-event `uuid` / `parentUuid` fields when transcoding from codex
/// (which has no per-event ids). Caller passes the parent view's id
/// and the line index within the source rollout; output is unique per
/// (parent, line).
pub fn synth_claude_uuid(parent_view_id: &str, line_index: usize) -> String {
    format!("{}-evt-{:08}", parent_view_id, line_index)
}

fn sanitize_for_id(s: &str) -> String {
    // Keep ascii alnum and dashes; collapse other chars to '-'.
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// Mint a fresh UUID-v4-shaped string for a brand-new segment on
/// cross-engine swap (or any case where anatta needs to pre-allocate an
/// engine_session_id before the engine spawns).
///
/// Codex accepts arbitrary ASCII for `session_meta.id` (verified by
/// spike); claude reads the file at `<projects>/<cwd>/<id>.jsonl`.
/// A standard UUID v4 string is filesystem-safe and visually compatible
/// with both engines' native ids.
pub fn mint_engine_session_id() -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let body: String = nanoid::nanoid!(30, &HEX);
    // Layout: xxxxxxxx-xxxx-4xxx-Yxxx-xxxxxxxxxxxx
    // where Y ∈ {8,9,a,b} per UUID v4 variant.
    format!(
        "{}-{}-4{}-{}{}-{}",
        &body[0..8],
        &body[8..12],
        &body[12..15],
        "8", // pick a fixed variant — codex/claude don't validate
        &body[15..18],
        &body[18..30],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_session_id_is_deterministic() {
        let a = view_session_id("019b8c2e-fe72", Engine::Claude);
        let b = view_session_id("019b8c2e-fe72", Engine::Claude);
        assert_eq!(a, b);
    }

    #[test]
    fn view_session_id_differs_by_target() {
        let a = view_session_id("source-x", Engine::Claude);
        let b = view_session_id("source-x", Engine::Codex);
        assert_ne!(a, b);
    }

    #[test]
    fn view_session_id_differs_by_source() {
        let a = view_session_id("a", Engine::Claude);
        let b = view_session_id("b", Engine::Claude);
        assert_ne!(a, b);
    }

    #[test]
    fn view_session_id_namespace_prefix() {
        let id = view_session_id("any", Engine::Codex);
        assert!(
            id.starts_with("anatta-view-"),
            "synthetic ids must be visibly namespaced; got {id}"
        );
    }

    #[test]
    fn view_sub_thread_id_distinct_per_index() {
        let parent = "anatta-view-X-codex";
        let a = view_sub_thread_id(parent, 0);
        let b = view_sub_thread_id(parent, 1);
        assert_ne!(a, b);
        assert_eq!(a, view_sub_thread_id(parent, 0)); // deterministic
    }

    #[test]
    fn map_tool_call_id_claude_to_codex_prefix() {
        let m = map_tool_call_id("toolu_abc123", Engine::Claude, Engine::Codex);
        assert_eq!(m, "anatta-cc-toolu_abc123");
    }

    #[test]
    fn map_tool_call_id_codex_to_claude_prefix() {
        let m = map_tool_call_id("call_xyz", Engine::Codex, Engine::Claude);
        assert_eq!(m, "anatta-cx-call_xyz");
    }

    #[test]
    fn map_tool_call_id_pairing_round_trips_within_target() {
        // The (call, result) pair must map identically so they still pair.
        let call = "toolu_pair";
        let mapped_call = map_tool_call_id(call, Engine::Claude, Engine::Codex);
        let mapped_result = map_tool_call_id(call, Engine::Claude, Engine::Codex);
        assert_eq!(mapped_call, mapped_result);
    }

    #[test]
    fn synth_claude_uuid_distinct_per_line() {
        let parent = "anatta-view-X-claude";
        let u0 = synth_claude_uuid(parent, 0);
        let u1 = synth_claude_uuid(parent, 1);
        assert_ne!(u0, u1);
        assert_eq!(u0, synth_claude_uuid(parent, 0));
    }

    #[test]
    fn mint_engine_session_id_shape() {
        let id = mint_engine_session_id();
        // 8-4-4-4-12 dashed, 36 chars total.
        assert_eq!(id.len(), 36, "got: {id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Two consecutive calls should not collide.
        let a = mint_engine_session_id();
        let b = mint_engine_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn sanitize_strips_path_chars() {
        let s = sanitize_for_id("/tmp/foo bar:baz");
        for c in s.chars() {
            assert!(c.is_ascii_alphanumeric() || c == '-', "non-id char survived: {c}");
        }
    }
}
