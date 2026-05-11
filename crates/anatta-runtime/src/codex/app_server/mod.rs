//! Codex app-server JSON-RPC client.
//!
//! `codex app-server --listen stdio://` is codex's JSON-RPC 2.0 protocol
//! (the same one powering the VS Code extension). Unlike `codex exec
//! --json`, it streams incremental `item/agentMessage/delta`
//! notifications token by token. anatta drives one app-server process
//! per chat turn:
//!
//! ```text
//!   spawn `codex app-server`
//!   → initialize / initialized (handshake)
//!   → thread/start or thread/resume   (capture thread.id = backend_session_id)
//!   → turn/start with the prompt
//!   → stream notifications via AppServerProjector → AgentEvent
//!   → on turn/completed: close stdin → server exits → child exits
//! ```
//!
//! Per-turn handshake adds ~200-500ms vs `codex exec --json`'s
//! one-shot model but gives us actual streaming and matches codex's
//! intended client integration surface.

pub(crate) mod projector;
pub(crate) mod wire;

pub(crate) use projector::AppServerProjector;
