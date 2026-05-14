//! Claude Code CLI wire-format types and distribution channel.
//!
//! Two distinct wire protocols, both supported as separate namespaces:
//!
//!   * [`history`] — disk session JSONL emitted by `claude` to
//!     `<CLAUDE_CONFIG_DIR>/projects/<cwd>/<session-uuid>.jsonl`.
//!     CamelCase envelope, rich metadata, includes `attachment` /
//!     `queue-operation` / `last-prompt` events but never `result` or
//!     `rate_limit_event` or `stream_event`.
//!
//!   * [`stream`] — stdout-only protocol emitted by
//!     `claude --print --output-format stream-json --verbose`.
//!     Snake_case envelope, minimal metadata, includes `system/init`,
//!     `system/status`, `result`, `rate_limit_event`, and (with
//!     `--include-partial-messages`) `stream_event`. Never persisted.

pub mod history;
pub mod projector;
pub mod sanitize;
pub mod stream;

pub use projector::{HistoryProjector, StreamProjector};
pub use sanitize::{SanitizeError, strip_reasoning};

#[cfg(feature = "installer")]
mod distribution;

#[cfg(feature = "installer")]
pub use distribution::ClaudeDistribution;
