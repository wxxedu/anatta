//! Agent CLI subprocess runtime for anatta.
//!
//! Hosts external agent CLIs (claude, codex) as child processes, parses their
//! NDJSON output into typed Rust events, and exposes a uniform spawn / stream
//! interface to the orchestrating layer.
//!
//! Each backend has two parsers and two projectors:
//!   * [`claude::history::ClaudeEvent`] + [`claude::HistoryProjector`]
//!   * [`claude::stream::ClaudeStreamEvent`] + [`claude::StreamProjector`]
//!   * [`codex::history::CodexEvent`]   + [`codex::HistoryProjector`]
//!   * [`codex::stream::CodexStreamEvent`] + [`codex::StreamProjector`]
//!
//! Parser enums are precise discriminated unions over the actual `type`
//! tags emitted on the wire. Unknown variants fail parsing deliberately,
//! so we notice when upstream adds something we haven't modeled.
//!
//! Projectors implement [`anatta_core::Projector`], which yields a
//! lossy stream of [`anatta_core::AgentEvent`] for the orchestrator,
//! UI, and HTTP/SSE layer to consume backend-agnostically.
//!
//! The [`profile`] module owns the per-Intent isolated configuration
//! directories that backends are launched against
//! ([`profile::ClaudeProfile`] / [`profile::CodexProfile`]).

pub mod claude;
pub mod codex;
pub mod profile;

/// Backend subprocess supervision. Gated behind the `spawn` feature so
/// pure-parser consumers don't pull in `tokio::process` / `tokio::sync`.
#[cfg(feature = "spawn")]
pub mod spawn;

/// Runtime version provisioning: download / verify / install agent CLI
/// binaries into anatta-controlled directories. Gated behind the
/// `installer` Cargo feature so spawn-only consumers (the daemon) don't
/// pay for HTTP/checksum/tar dependencies they never use.
#[cfg(feature = "installer")]
pub mod distribution;
