-- Conversation segments + family classification scaffolding.
--
-- Tier 1 introduces:
--   * `profile.family_override` for explicit Family opt-in/out
--   * conversations columns: id (ULID), backend (snapshot), session_uuid
--     (claude-assigned), created_at
--   * conversation_segments table — one row per profile-bound span of
--     a conversation's history.
--
-- Expand-only: legacy columns (conversations.profile_id,
-- conversations.backend_session_id) remain populated. They get retired
-- by a follow-up migration 0007 once all callers have switched.
--
-- ULIDs are backfilled in application code at startup (SQLite cannot
-- mint ULIDs natively). See `Store::backfill_for_0006`.

-- 1. profile.family_override (NULL = derive from (backend, provider))
ALTER TABLE profile ADD COLUMN family_override TEXT;

-- 2. conversations new columns
ALTER TABLE conversations ADD COLUMN id            TEXT;
ALTER TABLE conversations ADD COLUMN backend       TEXT;
ALTER TABLE conversations ADD COLUMN session_uuid  TEXT;
ALTER TABLE conversations ADD COLUMN created_at    TEXT;

-- 3. conversation_segments
--
-- Notes:
--   * conversation_id is a logical FK to conversations(id), but
--     conversations.id is not yet PK (still on `name`), so there is no
--     SQL FK constraint here. App enforces.
--   * source_family is FROZEN at creation: it captures
--     family_of(profile) at the moment the segment was opened.
--   * transition_policy is JSON-encoded SegmentRenderPolicy, e.g.
--     '{"kind":"verbatim"}' or '{"kind":"strip_reasoning"}'.
--   * last_absorbed_bytes / render_initial_bytes track the offset
--     into the (mutable) working file:
--       - On render: BOTH are set to the freshly-rendered file size.
--       - On absorb: last_absorbed_bytes advances to current file size;
--         render_initial_bytes does NOT change.
--     Crash recovery uses render_initial_bytes to compute the expected
--     central events.jsonl size (= last_absorbed_bytes - render_initial_bytes),
--     so a duplicate append is detectable and skippable.

CREATE TABLE conversation_segments(
    id                   TEXT PRIMARY KEY,
    conversation_id      TEXT NOT NULL,
    ordinal              INTEGER NOT NULL,
    profile_id           TEXT NOT NULL REFERENCES profile(id) ON DELETE RESTRICT,
    source_family        TEXT NOT NULL,
    started_at           TEXT NOT NULL,
    ended_at             TEXT,
    transition_policy    TEXT NOT NULL DEFAULT '{"kind":"verbatim"}',
    ended_with_compact   INTEGER NOT NULL DEFAULT 0,
    last_absorbed_bytes  INTEGER NOT NULL DEFAULT 0,
    render_initial_bytes INTEGER NOT NULL DEFAULT 0,
    UNIQUE (conversation_id, ordinal)
);

CREATE UNIQUE INDEX conversation_segments_one_active
    ON conversation_segments (conversation_id)
    WHERE ended_at IS NULL;

CREATE INDEX conversation_segments_by_conv
    ON conversation_segments (conversation_id, ordinal);
