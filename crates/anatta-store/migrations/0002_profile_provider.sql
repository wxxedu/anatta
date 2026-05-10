-- Provider routing on profiles.
--
-- `provider` identifies the API endpoint (anthropic, deepseek, kimi, ...).
-- Existing rows are backfilled to the natural default for their backend
-- (claude → anthropic, codex → openai). New inserts MUST supply provider.
--
-- The seven *_override columns let users override individual env-var
-- values that ProviderSpec defaults would otherwise contribute. NULL
-- means "use the spec default". Power users target a `custom` provider
-- when they need a fully bespoke base_url.
--
-- SQLite ALTER TABLE ADD COLUMN is the only ALTER form supported;
-- backfill runs as a follow-up UPDATE.

ALTER TABLE profile ADD COLUMN provider TEXT NOT NULL DEFAULT 'anthropic';
UPDATE profile SET provider = 'openai' WHERE backend = 'codex';

ALTER TABLE profile ADD COLUMN base_url_override             TEXT;
ALTER TABLE profile ADD COLUMN model_override                TEXT;
ALTER TABLE profile ADD COLUMN small_fast_model_override     TEXT;
ALTER TABLE profile ADD COLUMN default_opus_model_override   TEXT;
ALTER TABLE profile ADD COLUMN default_sonnet_model_override TEXT;
ALTER TABLE profile ADD COLUMN default_haiku_model_override  TEXT;
ALTER TABLE profile ADD COLUMN subagent_model_override       TEXT;

CREATE INDEX profile_provider_idx ON profile (provider);
