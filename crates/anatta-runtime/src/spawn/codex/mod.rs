//! Codex `app-server` (JSON-RPC 2.0 over stdio) launch.
//!
//! Two consumers:
//!
//! * `anatta send --resume <id>` (one-shot, single turn):
//!   `CodexLaunch::launch()` → `AgentSession`. Spawns app-server,
//!   handshakes, sends `turn/start`, drains, closes. Matches the
//!   `Launchable` contract used by all backends.
//!
//! * `anatta chat` (long-lived, many turns):
//!   `PersistentCodexSession::open()` → instance with `send_turn(prompt)`,
//!   plus a clone-friendly [`CodexInterruptHandle`] (via
//!   [`PersistentCodexSession::interrupt_handle`]) for cancelling the
//!   active turn from outside. The codex app-server stays alive for the
//!   entire chat session; each turn is just a `turn/start` request,
//!   eliminating the ~200-500ms handshake-per-turn cost and matching
//!   codex's intended client model (the VS Code extension does the same).
//!
//! Per-turn protocol (one-shot mode):
//!
//! ```text
//!   anatta                                 codex app-server
//!     --- initialize ---------------------------> id=0
//!     <--- initialize result --------------------
//!     --- initialized (notification) ----------->
//!     --- thread/start or thread/resume --------> id=1
//!     <--- thread/start result (thread.id) ------
//!     --- turn/start (prompt) ------------------> id=2
//!     <--- turn/start result --------------------
//!     [<--- turn/started ... turn/completed ----] (notifications)
//!     --- (close stdin) ------------------------>  graceful shutdown
//! ```
//!
//! In persistent mode the first three steps run at `open()`; each
//! subsequent `send_turn` only does `turn/start` + drain.
//!
//! ## Module layout
//!
//! * [`launch`] — one-shot `CodexLaunch` + `Launchable` impl.
//! * [`persistent`] — long-lived `PersistentCodexSession` for chat,
//!   plus `CodexInterruptHandle` / `TurnHandle`.
//! * [`handshake`] — shared initialize/thread-start sequence used by
//!   both one-shot and persistent flows.
//! * [`pump`] — stdout reader loops + JSON-RPC I/O + event-builder
//!   helpers shared by both flows.

mod handshake;
mod launch;
mod persistent;
mod pump;

pub use launch::CodexLaunch;
pub use persistent::{CodexInterruptHandle, PersistentCodexSession, TurnHandle};

/// Codex `approval_policy` value passed in `turn/start` requests and in
/// the on-disk `turn_context.payload`. Anatta orchestrates approvals
/// itself; codex must not prompt.
const APPROVAL_POLICY: &str = "never";

/// Codex `sandbox` policy passed to `thread/start.sandbox`. Anatta runs
/// in user-controlled worktrees and supplies its own filesystem
/// isolation, so codex's sandbox is disabled.
const SANDBOX_POLICY: &str = "danger-full-access";

/// JSON-RPC request id used for the first `turn/start` of a session.
/// One-shot launches use this once; persistent sessions allocate ids
/// monotonically from here.
const FIRST_TURN_REQUEST_ID: i64 = 2;
