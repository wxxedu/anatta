-- Tier 3 cross-engine swap: additive columns + backfill state marker.
--
-- Step 1 of two-step migration. Step 2 (0007b destructive DROP) is run
-- via Rust orchestration AFTER:
--   (a) this additive migration applied
--   (b) Rust backfill copies profile.backend → segment.backend and
--       conversations.session_uuid → ordinal-0 segment.engine_session_id
--   (c) the marker row in anatta_migration_state confirms backfill done
--
-- See docs/superpowers/specs/2026-05-13-cross-engine-swap-design.md
-- §Migration for the orchestration sequence.

-- 1. New columns on conversation_segments.
--    backend: the producing engine for this segment ('claude' | 'codex').
--             DEFAULT 'claude' so the ALTER succeeds on existing rows;
--             backfill then sets the correct value per row.
--    engine_session_id: claude sessionId or codex thread_id; NULL until
--             the segment's first turn produces one. For ordinal=0
--             segments, the backfill seeds this from the legacy
--             conversations.session_uuid.
ALTER TABLE conversation_segments
    ADD COLUMN backend TEXT NOT NULL DEFAULT 'claude';

ALTER TABLE conversation_segments
    ADD COLUMN engine_session_id TEXT;

-- 2. Marker table for the Rust migration driver.
CREATE TABLE IF NOT EXISTS anatta_migration_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
