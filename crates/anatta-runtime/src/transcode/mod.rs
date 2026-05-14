//! Cross-engine transcoder.
//!
//! Tier 3 (cross-engine swap). Source canonical lives in the producing
//! engine's native wire format. When a target engine needs to render
//! prior history, [`transcode_to`] produces a per-target view (cached
//! at the central segment store under `views/<engine>/`).
//!
//! Two directions:
//!   * [`claude_to_codex`] — reads claude history JSONL, emits codex
//!     rollout JSONL.
//!   * [`codex_to_claude`] — reads codex rollout JSONL, emits claude
//!     history JSONL.
//!
//! Both drop reasoning blocks unconditionally (signatures /
//! encrypted_content are vendor-bound and cannot be re-validated by
//! the other engine). Text + tool calls + tool results map
//! structurally.
//!
//! Sub-agent recursion (claude `Task` ↔ codex sub-thread spawn) is
//! gated on a future spike result and is not implemented in v1; the
//! v1 fallback flattens sub-agent activity into a single
//! `tool_call`/`tool_result` pair carrying the sub's final message.

pub mod cache;
pub mod claude_to_codex;
pub mod codex_to_claude;
pub mod id_mint;

pub use cache::{CacheLookup, SegmentLocation, resolve_for_target};

use std::path::{Path, PathBuf};

/// Bumped when any mapping rule, id-minting algorithm, or new variant
/// changes. View caches with a different version are rebuilt.
pub const TRANSCODER_VERSION: u32 = 1;

/// Which engine produced the source events, and which engine the view
/// is being built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Claude,
    Codex,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Claude => "claude",
            Engine::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Engine::Claude),
            "codex" => Some(Engine::Codex),
            _ => None,
        }
    }
}

/// Inputs to one transcode call.
#[derive(Debug, Clone)]
pub struct TranscodeInput<'a> {
    pub source_engine: Engine,
    pub source_events_jsonl: &'a Path,
    /// May not exist (e.g., codex segments without sub-agents).
    pub source_sidecar_dir: &'a Path,
    /// Used to derive the deterministic synthetic id of the view.
    pub source_engine_session_id: &'a str,
    /// Used by codex preamble (session_meta.cwd).
    pub conversation_cwd: &'a str,
}

/// Output of one transcode call.
#[derive(Debug, Clone)]
pub struct TranscodeOutput {
    pub view_engine_session_id: String,
    pub view_events_path: PathBuf,
    pub view_sidecar_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("source events.jsonl malformed at line {line}: {source}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("missing sub-agent transcript: {path}")]
    MissingSubAgent { path: PathBuf },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported source variant: {0}")]
    Unsupported(String),
    #[error("source events.jsonl is empty (no session header)")]
    EmptySource,
}

/// Transcode a segment's source-native events into target-engine view shape.
///
/// Writes to a `<view_dir>.tmp` directory and atomically renames to
/// `<view_dir>` on success. On error, the `.tmp` directory is removed
/// and `<view_dir>` is left untouched.
pub fn transcode_to(
    target: Engine,
    input: TranscodeInput<'_>,
    view_dir: &Path,
) -> Result<TranscodeOutput, TranscodeError> {
    match (input.source_engine, target) {
        (Engine::Claude, Engine::Claude) | (Engine::Codex, Engine::Codex) => {
            // Same-engine transcode is a no-op (caller should not invoke).
            Err(TranscodeError::Unsupported(format!(
                "transcode_to called with source == target ({:?})",
                target
            )))
        }
        (Engine::Claude, Engine::Codex) => claude_to_codex::run(input, view_dir),
        (Engine::Codex, Engine::Claude) => codex_to_claude::run(input, view_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_parses_and_stringifies() {
        assert_eq!(Engine::parse("claude"), Some(Engine::Claude));
        assert_eq!(Engine::parse("codex"), Some(Engine::Codex));
        assert_eq!(Engine::parse("gpt"), None);
        assert_eq!(Engine::Claude.as_str(), "claude");
        assert_eq!(Engine::Codex.as_str(), "codex");
    }

    #[test]
    fn same_engine_transcode_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let input_path = tmp.path().join("events.jsonl");
        std::fs::write(&input_path, "").unwrap();
        let sidecar = tmp.path().join("sidecar");
        let view_dir = tmp.path().join("view");

        let err = transcode_to(
            Engine::Claude,
            TranscodeInput {
                source_engine: Engine::Claude,
                source_events_jsonl: &input_path,
                source_sidecar_dir: &sidecar,
                source_engine_session_id: "same",
                conversation_cwd: "/tmp",
            },
            &view_dir,
        )
        .unwrap_err();
        assert!(matches!(err, TranscodeError::Unsupported(_)));
    }
}
