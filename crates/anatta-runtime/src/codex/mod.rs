//! Codex CLI wire-format types and distribution channel.
//!
//! Two distinct wire protocols, both supported as separate namespaces:
//!
//!   * [`history`] — disk rollout JSONL emitted by `codex` to
//!     `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//!     Lower-level event_msg / response_item model with full envelope
//!     (`session_meta`, `turn_context`, ...).
//!
//!   * [`stream`] — stdout-only protocol emitted by
//!     `codex exec --json`. Higher-level thread/turn/item model
//!     (`thread.started`, `turn.started`, `item.completed`,
//!     `turn.completed`). Never persisted.

pub mod history;
pub mod stream;
pub mod projector;

pub use projector::{HistoryProjector, StreamProjector};

#[cfg(feature = "installer")]
mod distribution;

#[cfg(feature = "installer")]
pub use distribution::CodexDistribution;
