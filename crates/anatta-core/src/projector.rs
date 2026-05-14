//! Projector trait — the contract for transforming any backend's raw
//! event into a stream of [`AgentEvent`].
//!
//! Each backend (claude, codex) provides two implementations: one for
//! its disk session JSONL and one for its stdout streaming protocol.
//! Stateless implementations are zero-sized structs; stateful ones
//! (claude's incremental SSE) carry per-content-block accumulators.
//!
//! Spawn supervisors and similar backend-agnostic code can write
//! `fn run<P: Projector>(p: P, ...)` to consume any backend uniformly.

use chrono::{DateTime, Utc};

use crate::AgentEvent;

/// Lossy one-way projection from a backend's raw event to
/// `AgentEvent`s. A single raw event may produce 0, 1, or many
/// `AgentEvent`s.
pub trait Projector {
    /// The raw event type this projector consumes (e.g. `ClaudeEvent`,
    /// `CodexStreamEvent`).
    type Raw;

    /// Project one raw event into 0+ `AgentEvent`s. `&mut self` so
    /// stateful projectors can accumulate snapshot state across calls.
    fn project(&mut self, raw: &Self::Raw, ctx: &ProjectionContext) -> Vec<AgentEvent>;
}

/// Caller-supplied defaults for fields that some raw events lack
/// (notably stream events without a session_id or timestamp).
/// Projectors override with raw-event values where available.
#[derive(Debug, Clone)]
pub struct ProjectionContext {
    pub session_id: String,
    pub received_at: DateTime<Utc>,
}
