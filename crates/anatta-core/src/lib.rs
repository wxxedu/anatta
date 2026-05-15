//! Pure domain logic for anatta.
//!
//! This crate is the home of the Intent finite state machine, the
//! inter-intent dependency graph, the Card and Guard state machines
//! (Phase 2), and the unified [`AgentEvent`] semantic event type.
//! It deliberately has no IO, no async, and no networking — every
//! function here should be a pure transformation over plain Rust
//! values.

pub mod agent_event;
pub mod permission;
pub mod projector;

pub use agent_event::{AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend};
pub use permission::PermissionLevel;
pub use projector::{ProjectionContext, Projector};
