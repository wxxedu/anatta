//! Unified semantic event stream consumed by the orchestrator.
//!
//! `AgentEvent` is the *lossy* projection of any of the four backend
//! wire formats (claude history/stream, codex history/stream) into a
//! single shape that:
//!   - the card-stream UI can render directly,
//!   - the guard DAG can dispatch over,
//!   - the daemon ↔ server gRPC pipe can transport without dragging
//!     in 50+ raw event variants.
//!
//! Projection is *one-way only*: raw → unified. The envelope keeps a
//! `raw_uuid` back-pointer so that consumers needing forensics /
//! replay can look up the original raw event by `(session_id, raw_uuid)`
//! against the daemon's local jsonl store. We never serialize the raw
//! payload here.
//!
//! Streaming: each finalized variant (`AssistantText`, `Thinking`,
//! `ToolUse`) has a parallel `*Delta` variant for intermediate
//! snapshots emitted by future streaming projections (claude
//! `--include-partial-messages`, codex item updates). Delta variants
//! carry **snapshot** state (`text_so_far` = full accumulated text up
//! to that point), not increments — so a UI consumer can render
//! whatever delta it last saw and a dropped delta is self-healing.
//! `ToolUseInputDelta.partial_json` is the one exception: it carries
//! raw incomplete JSON because half-parsed input has no Rust value yet.
//!
//! v1 projections emit only the finalized variants. The delta variants
//! are part of the wire format from day one so future streaming UIs
//! don't require a schema break.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentEvent {
    pub envelope: AgentEventEnvelope,
    pub payload: AgentEventPayload,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentEventEnvelope {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub backend: Backend,
    /// UUID of the raw event this projection came from. Use for forensics
    /// against the daemon-local raw jsonl store.
    pub raw_uuid: Option<String>,
    /// Set on events emitted from a sub-agent (Task tool / collab tool call).
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEventPayload {
    SessionStarted {
        cwd: String,
        model: String,
        #[serde(default)]
        tools_available: Vec<String>,
    },
    TurnStarted,
    UserPrompt {
        text: String,
    },
    /// Finalized assistant text — the content block has stopped streaming.
    AssistantText {
        text: String,
    },
    /// In-progress assistant text snapshot. `text_so_far` is the full
    /// accumulated content up to this point, not an increment.
    AssistantTextDelta {
        content_block_index: u32,
        text_so_far: String,
    },
    /// Finalized thinking block.
    Thinking {
        text: String,
    },
    /// In-progress thinking snapshot. `text_so_far` is full accumulated
    /// thinking up to this point.
    ThinkingDelta {
        content_block_index: u32,
        text_so_far: String,
    },
    /// Finalized tool invocation. `input` is the parsed structured value.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// In-progress tool input being assembled. `partial_json` is the raw
    /// fragment-so-far (cannot be parsed yet); useful for "agent is typing
    /// arguments" UI state. The corresponding `ToolUse` will fire when the
    /// content block stops.
    ToolUseInputDelta {
        tool_use_id: String,
        content_block_index: u32,
        partial_json: String,
    },
    ToolResult {
        tool_use_id: String,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<Value>,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cache_read: u64,
        #[serde(default)]
        cache_create: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    TurnCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        is_error: bool,
    },
    RateLimit {
        /// Window kind. Claude uses `"five_hour"` / `"seven_day"`; codex
        /// uses `"primary"` / `"secondary"` (the snapshot exposes two
        /// rolling windows side-by-side rather than naming them).
        limit_kind: String,
        /// Percentage of the window consumed, normalized to 0–100 across
        /// backends (claude's wire-level `utilization` is 0.0–1.0; codex
        /// already emits 0–100). `None` means the backend didn't include
        /// this in the event — not "0%".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_percent: Option<f64>,
        /// Status of this window: `"ok"` (under limit), `"warning"`
        /// (claude `allowed_warning` / approaching), `"rejected"` (limit
        /// hit). Codex doesn't emit a per-window status; we synthesize
        /// `"rejected"` when its top-level `rate_limit_reached_type` is
        /// set, otherwise leave `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        using_overage: Option<bool>,
    },
    Error {
        message: String,
        fatal: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_assistant_text_event() {
        let ev = AgentEvent {
            envelope: AgentEventEnvelope {
                session_id: "s".into(),
                timestamp: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                backend: Backend::Claude,
                raw_uuid: Some("u".into()),
                parent_tool_use_id: None,
            },
            payload: AgentEventPayload::AssistantText {
                text: "hello".into(),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: AgentEvent = serde_json::from_str(&s).unwrap();
        match back.payload {
            AgentEventPayload::AssistantText { text } => assert_eq!(text, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn delta_variants_have_distinct_kinds() {
        let p1 = AgentEventPayload::AssistantTextDelta {
            content_block_index: 0,
            text_so_far: "He".into(),
        };
        let p2 = AgentEventPayload::ToolUseInputDelta {
            tool_use_id: "t1".into(),
            content_block_index: 0,
            partial_json: "{\"cm".into(),
        };
        let s1 = serde_json::to_string(&p1).unwrap();
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(s1.contains("assistant_text_delta"));
        assert!(s2.contains("tool_use_input_delta"));
    }

    #[test]
    fn payload_kind_tag_present_in_json() {
        let p = AgentEventPayload::TurnStarted;
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kind\":\"turn_started\""), "got: {s}");
    }
}
