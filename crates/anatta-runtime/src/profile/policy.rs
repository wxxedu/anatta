//! Render-time policy for segment ingestion.
//!
//! When a new segment is opened, prior segments' events are rendered into
//! the new segment's working file. Each prior segment's `source_family`
//! and the new segment's profile family together determine the policy to
//! apply.
//!
//! Tier 1 implements only `Verbatim` and `StripReasoning` at render time.
//! Other variants (`Compact`, `Drop`, `ToolsOnly`) are part of the data
//! model for forward compatibility; render will reject them in tier 1.

use serde::{Deserialize, Serialize};

use super::family::Family;

/// The transformation applied to one prior segment's events when
/// rendering them into a new segment's working file.
///
/// Serialized as `{"kind":"...","..."}` in `conversation_segments.transition_policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SegmentRenderPolicy {
    /// Byte-copy: prior events emitted unchanged. Default for same-family
    /// transitions and strict → lax.
    Verbatim,

    /// Drop thinking-only assistant events; rewire the DAG's `parentUuid`
    /// locally. Required for lax → strict transitions.
    StripReasoning,

    /// **Reserved (not implemented in tier 1)**: replace prior segments
    /// with a synthesized summary. Tier 1 achieves this via the
    /// `compact_before_close` transition hook, not via this render
    /// policy. The data model accepts this variant for future tiers.
    Compact { summary: CompactSummary },

    /// **Reserved (not implemented in tier 1)**: drop the prior segment
    /// entirely. "Forget that interlude happened."
    Drop,

    /// **Reserved (not implemented in tier 1)**: keep only tool calls
    /// and their results; drop conversational text + thinking.
    ToolsOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactSummary {
    Cached(String),
    LazyByTargetModel,
    LazyByProfile { profile_id: String },
}

impl SegmentRenderPolicy {
    /// True iff this policy is implemented at render time in tier 1.
    /// Render will return an error for unimplemented variants.
    pub fn is_tier1_render(&self) -> bool {
        matches!(self, Self::Verbatim | Self::StripReasoning)
    }
}

/// Minimum required policy when going from `src` family to `dst` family.
///
/// Users may opt UP (e.g., `Compact` to visually collapse a noisy
/// segment) but cannot opt DOWN below this floor.
pub fn min_policy_for(src: Family, dst: Family) -> SegmentRenderPolicy {
    if Family::needs_sanitize(src, dst) {
        SegmentRenderPolicy::StripReasoning
    } else {
        SegmentRenderPolicy::Verbatim
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::family::Family;

    #[test]
    fn verbatim_serde() {
        let p = SegmentRenderPolicy::Verbatim;
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"kind":"verbatim"}"#);
        let back: SegmentRenderPolicy = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn strip_reasoning_serde() {
        let p = SegmentRenderPolicy::StripReasoning;
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"kind":"strip_reasoning"}"#);
        let back: SegmentRenderPolicy = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn min_policy_matrix() {
        // lax → strict: strip
        assert_eq!(
            min_policy_for(Family::ACompat, Family::ANative),
            SegmentRenderPolicy::StripReasoning,
        );
        // strict → lax: verbatim
        assert_eq!(
            min_policy_for(Family::ANative, Family::ACompat),
            SegmentRenderPolicy::Verbatim,
        );
        // same family: verbatim
        assert_eq!(
            min_policy_for(Family::ANative, Family::ANative),
            SegmentRenderPolicy::Verbatim,
        );
        assert_eq!(
            min_policy_for(Family::ACompat, Family::ACompat),
            SegmentRenderPolicy::Verbatim,
        );
    }

    #[test]
    fn tier1_implementation_flag() {
        assert!(SegmentRenderPolicy::Verbatim.is_tier1_render());
        assert!(SegmentRenderPolicy::StripReasoning.is_tier1_render());
        assert!(!SegmentRenderPolicy::Drop.is_tier1_render());
        assert!(!SegmentRenderPolicy::ToolsOnly.is_tier1_render());
        let compact = SegmentRenderPolicy::Compact {
            summary: CompactSummary::LazyByTargetModel,
        };
        assert!(!compact.is_tier1_render());
    }
}
