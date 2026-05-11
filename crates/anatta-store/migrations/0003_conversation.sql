-- Per-name multi-turn chat sessions for `anatta chat`.
--
-- One row = one named conversation against a single profile. The
-- `backend_session_id` is the claude session UUID / codex thread UUID
-- used with `--resume`; NULL until the first turn produces it.
--
-- `lock_holder_pid` enforces single-writer access from `anatta chat`
-- against the underlying claude/codex on-disk session file. NULL = idle.
-- Acquire/release runs through `try_acquire_with_check` (see
-- `conversation.rs`). PID liveness is a CLI-side callback (libc::kill).

CREATE TABLE conversations (
    name               TEXT NOT NULL PRIMARY KEY,
    profile_id         TEXT NOT NULL REFERENCES profile(id) ON DELETE RESTRICT,
    backend_session_id TEXT,
    cwd                TEXT NOT NULL,
    last_used_at       TEXT NOT NULL,
    lock_holder_pid    INTEGER
);
