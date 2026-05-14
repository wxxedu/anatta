//! Rust-orchestrated parts of the tier-3 cross-engine migration.
//!
//! `sqlx::migrate!` runs the SQL migrations in `./migrations/` in order
//! at startup, but tier 3 needs Rust logic to run between the additive
//! step (0007) and the destructive step. The flow is:
//!
//! 1. `sqlx::migrate!` applies `0007_cross_engine_additive.sql`
//!    (adds `backend` and `engine_session_id` columns, creates the
//!    `anatta_migration_state` marker table).
//! 2. [`run_tier3_post_migration`] runs the Rust backfill + the
//!    Rust-guarded destructive DROP. Both phases are idempotent and
//!    use marker rows in `anatta_migration_state` to avoid re-running.
//!
//! Phase 2 lives here because:
//!   * `RAISE(ABORT, ...)` only works inside SQLite triggers, so a
//!     `SELECT CASE WHEN ... THEN RAISE(...)` guard in a plain
//!     migration is invalid syntax.
//!   * The backfill needs application data (the segment row's
//!     `profile_id` joined to `profile.backend`), so it can't be a
//!     pure SQL function.

use sqlx::SqlitePool;

use crate::StoreError;

const KEY_BACKFILL_DONE: &str = "0007_backfill";
const KEY_DROP_DONE: &str = "0007_drop";

/// Run the post-SQL-migration phase of the tier-3 work. Idempotent.
///
/// Called from [`crate::Store::open`] after `MIGRATOR.run` returns.
/// Holds the pool throughout; callers should ensure no other writer
/// is touching the DB (the existing migration lock on
/// `<anatta_home>/runtime-locks/migration.lock` covers this in the
/// production binaries).
pub(crate) async fn run_tier3_post_migration(pool: &SqlitePool) -> Result<(), StoreError> {
    // Skip entirely if the marker table doesn't exist (e.g., a future
    // migration removed it, or running against a pre-0007 DB during
    // development).
    if !marker_table_exists(pool).await? {
        return Ok(());
    }

    if !is_done(pool, KEY_BACKFILL_DONE).await? {
        precheck_no_orphan_profile_ids(pool).await?;
        backfill_segments_backend(pool).await?;
        backfill_first_segment_engine_session_id(pool).await?;
        mark_done(pool, KEY_BACKFILL_DONE).await?;
    }

    if !is_done(pool, KEY_DROP_DONE).await? {
        // Wait for an explicit opt-in to run the destructive DROP.
        // (The drop runs only when the calling binary sets the marker
        // via `enable_destructive_drop`; the default Store::open path
        // leaves the legacy conversations.{backend,session_uuid}
        // columns in place until the user is ready.)
        if !is_done(pool, "0007_drop_enabled").await? {
            return Ok(());
        }
        ensure_no_null_backend_rows(pool).await?;
        execute_destructive_drop(pool).await?;
        mark_done(pool, KEY_DROP_DONE).await?;
    }

    Ok(())
}

/// Opt-in toggle for the destructive `ALTER TABLE conversations DROP COLUMN`
/// step. Once set, the next call to [`run_tier3_post_migration`] will
/// perform the DROP if all preconditions are met. Idempotent.
pub async fn enable_destructive_drop(pool: &SqlitePool) -> Result<(), StoreError> {
    if !marker_table_exists(pool).await? {
        // No anatta_migration_state table yet (pre-0007 DB). Caller
        // should re-run after open() to pick up the table.
        return Err(StoreError::MigrationBlocked(
            "anatta_migration_state table missing; run Store::open() first to apply 0007a"
                .to_owned(),
        ));
    }
    mark_done(pool, "0007_drop_enabled").await
}

async fn marker_table_exists(pool: &SqlitePool) -> Result<bool, StoreError> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='anatta_migration_state'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

async fn is_done(pool: &SqlitePool, key: &str) -> Result<bool, StoreError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM anatta_migration_state WHERE key = ?")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(v,)| v == "done").unwrap_or(false))
}

async fn mark_done(pool: &SqlitePool, key: &str) -> Result<(), StoreError> {
    sqlx::query("INSERT OR REPLACE INTO anatta_migration_state (key, value) VALUES (?, 'done')")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

async fn precheck_no_orphan_profile_ids(pool: &SqlitePool) -> Result<(), StoreError> {
    let orphans: Vec<(String,)> = sqlx::query_as(
        "SELECT cs.id
           FROM conversation_segments cs
      LEFT JOIN profile p ON p.id = cs.profile_id
          WHERE p.id IS NULL",
    )
    .fetch_all(pool)
    .await?;
    if !orphans.is_empty() {
        let ids: Vec<String> = orphans.into_iter().map(|(s,)| s).collect();
        return Err(StoreError::MigrationBlocked(format!(
            "{} conversation_segments rows reference missing profiles: [{}]. \
             Resolve manually (delete orphan segments or restore profiles) and re-run.",
            ids.len(),
            ids.join(", "),
        )));
    }
    Ok(())
}

async fn backfill_segments_backend(pool: &SqlitePool) -> Result<(), StoreError> {
    // Set backend on every segment from its profile's backend. Idempotent:
    // running twice writes the same value. We update unconditionally rather
    // than gating on `WHERE backend = 'claude'` because 'claude' could be a
    // legitimate value (the DEFAULT 'claude' applied to existing rows
    // pre-backfill happens to be correct for those rows whose profile is
    // claude, but we re-derive to be precise).
    sqlx::query(
        "UPDATE conversation_segments
            SET backend = (
                SELECT backend FROM profile WHERE profile.id = conversation_segments.profile_id
            )
          WHERE EXISTS (
                SELECT 1 FROM profile WHERE profile.id = conversation_segments.profile_id
          )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn backfill_first_segment_engine_session_id(pool: &SqlitePool) -> Result<(), StoreError> {
    // For each conversation that has a legacy `session_uuid`, copy it into
    // the ordinal-0 segment's `engine_session_id` (and ONLY if that segment
    // doesn't already have one). Idempotent.
    sqlx::query(
        "UPDATE conversation_segments
            SET engine_session_id = (
                SELECT c.session_uuid
                  FROM conversations c
                 WHERE c.id = conversation_segments.conversation_id
                   AND c.session_uuid IS NOT NULL
            )
          WHERE ordinal = 0
            AND engine_session_id IS NULL
            AND EXISTS (
                SELECT 1 FROM conversations c
                 WHERE c.id = conversation_segments.conversation_id
                   AND c.session_uuid IS NOT NULL
            )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_no_null_backend_rows(pool: &SqlitePool) -> Result<(), StoreError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM conversation_segments WHERE backend IS NULL OR backend = ''",
    )
    .fetch_one(pool)
    .await?;
    if row.0 > 0 {
        return Err(StoreError::MigrationBlocked(format!(
            "{} conversation_segments rows have NULL/empty backend; backfill incomplete",
            row.0
        )));
    }
    Ok(())
}

async fn execute_destructive_drop(pool: &SqlitePool) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("ALTER TABLE conversations DROP COLUMN backend")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ALTER TABLE conversations DROP COLUMN session_uuid")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[tokio::test]
    async fn open_runs_post_migration_idempotently() {
        // Just opening twice in a row should not crash or double-apply.
        let store = Store::open_in_memory().await.unwrap();
        // Second call on the same pool (via a fresh `run_tier3_post_migration`)
        // should still succeed.
        run_tier3_post_migration(store.pool()).await.unwrap();
        run_tier3_post_migration(store.pool()).await.unwrap();
    }

    #[tokio::test]
    async fn backfill_sets_segment_backend_from_profile() {
        let store = Store::open_in_memory().await.unwrap();

        // Insert a codex profile and a claude profile.
        sqlx::query(
            "INSERT INTO profile (id, backend, name, auth_method, created_at, provider)
             VALUES ('codex-XYZ', 'codex', 'cod', 'login', '2026-01-01T00:00:00Z', 'openai'),
                    ('claude-AAA', 'claude', 'cla', 'login', '2026-01-01T00:00:00Z', 'anthropic')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        // Insert two segments — one against each profile.
        // The default backend column value is 'claude'; the backfill should
        // overwrite for the codex one.
        sqlx::query(
            "INSERT INTO conversation_segments
               (id, conversation_id, ordinal, profile_id, source_family, started_at, transition_policy, backend)
             VALUES
               ('seg-cx', 'conv-1', 0, 'codex-XYZ', 'o-native', '2026-01-01T00:00:00Z', '{}', 'claude'),
               ('seg-cc', 'conv-2', 0, 'claude-AAA', 'a-native', '2026-01-01T00:00:00Z', '{}', 'claude')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        // Re-run backfill (it ran once on open; manually re-run to verify
        // the codex row gets the right backend).
        // Wipe the marker so we don't short-circuit.
        sqlx::query("DELETE FROM anatta_migration_state WHERE key = '0007_backfill'")
            .execute(store.pool())
            .await
            .unwrap();
        run_tier3_post_migration(store.pool()).await.unwrap();

        let row: (String,) =
            sqlx::query_as("SELECT backend FROM conversation_segments WHERE id = 'seg-cx'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "codex");

        let row: (String,) =
            sqlx::query_as("SELECT backend FROM conversation_segments WHERE id = 'seg-cc'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "claude");
    }

    #[tokio::test]
    async fn orphan_profile_id_aborts_backfill() {
        let store = Store::open_in_memory().await.unwrap();

        // Disable FK temporarily so we can insert an orphan row to test
        // the backfill's precheck. In production an orphan would require
        // PRAGMA foreign_keys=OFF + manual editing, but the precheck
        // still catches it on next anatta startup.
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(store.pool())
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO conversation_segments
               (id, conversation_id, ordinal, profile_id, source_family, started_at, transition_policy)
             VALUES
               ('seg-orphan', 'conv-1', 0, 'missing-profile', 'a-native', '2026-01-01T00:00:00Z', '{}')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(store.pool())
            .await
            .unwrap();

        sqlx::query("DELETE FROM anatta_migration_state WHERE key = '0007_backfill'")
            .execute(store.pool())
            .await
            .unwrap();

        let err = run_tier3_post_migration(store.pool()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("seg-orphan"),
            "expected orphan id in error: {msg}"
        );
    }

    #[tokio::test]
    async fn enable_destructive_drop_then_run_drops_columns() {
        let store = Store::open_in_memory().await.unwrap();

        // Pre-conditions: profile + segment with backend set.
        sqlx::query(
            "INSERT INTO profile (id, backend, name, auth_method, created_at, provider)
             VALUES ('claude-AAA', 'claude', 'a', 'login', '2026-01-01T00:00:00Z', 'anthropic')",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_segments
               (id, conversation_id, ordinal, profile_id, source_family, started_at, transition_policy, backend)
             VALUES
               ('seg-1', 'conv-1', 0, 'claude-AAA', 'a-native', '2026-01-01T00:00:00Z', '{}', 'claude')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        enable_destructive_drop(store.pool()).await.unwrap();
        run_tier3_post_migration(store.pool()).await.unwrap();

        // conversations.backend should be gone.
        let res = sqlx::query("SELECT backend FROM conversations")
            .execute(store.pool())
            .await;
        assert!(
            res.is_err(),
            "conversations.backend should have been dropped"
        );
    }
}
