-- Initial profile table.
--
-- Each row represents a per-account configuration of a backend CLI
-- (claude / codex). The on-disk side (CLAUDE_CONFIG_DIR /
-- CODEX_HOME directory + symlinks) is created by anatta-runtime;
-- this table holds the user-facing metadata that maps the prefix-
-- qualified id to a human-friendly name.

CREATE TABLE profile (
    id           TEXT NOT NULL PRIMARY KEY,    -- "claude-AbCd1234" / "codex-XyZw9876"
    backend      TEXT NOT NULL,                -- "claude" | "codex"
    name         TEXT NOT NULL,                -- user-facing label (mutable)
    auth_method  TEXT NOT NULL,                -- "login" | "api_key"
    created_at   TEXT NOT NULL,                -- RFC3339 UTC
    last_used_at TEXT,                         -- RFC3339 UTC, nullable

    UNIQUE (backend, name)
);

CREATE INDEX profile_backend_idx ON profile (backend);
