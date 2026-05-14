//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).
//!
//! Mirrors the long-lived shape of
//! [`PersistentCodexSession`](crate::spawn::PersistentCodexSession): one
//! `open()`, many `send_turn()` calls, one `close()`.

// implementation lands in later tasks
