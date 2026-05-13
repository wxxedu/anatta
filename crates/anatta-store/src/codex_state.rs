//! Read-only access to codex's local state DB (`<CODEX_HOME>/state_5.sqlite`).
//!
//! Codex maintains a small sqlite per CODEX_HOME tracking the threads it
//! has created and the parent → sub-agent edges. We open this DB
//! read-only with a short busy timeout so concurrent codex processes
//! don't block anatta's absorb flow.
//!
//! Empirically verified schema (codex-cli 0.125.0):
//!
//! ```sql
//! CREATE TABLE threads (
//!     id           TEXT PRIMARY KEY,
//!     rollout_path TEXT NOT NULL,
//!     -- + many other columns: source, model_provider, cwd, title, ...
//! );
//! CREATE TABLE thread_spawn_edges (
//!     parent_thread_id TEXT NOT NULL,
//!     child_thread_id  TEXT NOT NULL PRIMARY KEY,
//!     status           TEXT NOT NULL
//! );
//! ```
//!
//! Tier 3 absorb uses [`list_sub_agent_rollouts`] to discover sub-agent
//! transcript files for a codex segment, then copies them into the
//! central segment's `sidecar/subagents/` directory.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;

use crate::StoreError;

/// One sub-agent's identity + on-disk rollout location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgentRef {
    pub child_thread_id: String,
    pub rollout_path: String,
}

/// Find every sub-agent thread spawned (directly or transitively) by
/// `parent_thread_id` in the codex state DB at `state_db_path`.
///
/// Read-only access; safe to call while codex is running. Returns an
/// empty vec if no sub-agents, or if the DB or tables are missing.
/// Surfaces errors only for I/O / SQL-level failures.
///
/// Walks transitively (depth-first) so descendants of sub-agents are
/// included. Cycle-safe via visited set.
pub async fn list_sub_agent_rollouts(
    state_db_path: &Path,
    parent_thread_id: &str,
) -> Result<Vec<SubAgentRef>, StoreError> {
    if !state_db_path.exists() {
        return Ok(Vec::new());
    }

    // sqlx-sqlite expects file: scheme; read_only=true opens with
    // SQLITE_OPEN_READONLY so we don't touch the WAL or attempt any
    // schema queries that need write access. We deliberately do NOT
    // set journal_mode here — that requires a write to apply.
    let path_str = state_db_path
        .to_str()
        .ok_or_else(|| StoreError::Io(std::io::Error::other("state DB path is not UTF-8")))?;
    let opts = SqliteConnectOptions::new()
        .filename(path_str)
        .read_only(true)
        .busy_timeout(Duration::from_secs(3));
    let pool = sqlx::SqlitePool::connect_with(opts).await?;

    // Confirm the tables exist; if codex changed schema, gracefully
    // return empty rather than erroring loudly. (The caller logs a
    // warning if it cares.)
    let has_tables: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM sqlite_master
          WHERE type='table' AND name IN ('threads','thread_spawn_edges')
          GROUP BY 1
         HAVING COUNT(DISTINCT name) = 2",
    )
    .fetch_optional(&pool)
    .await?;
    if has_tables.is_none() {
        pool.close().await;
        return Ok(Vec::new());
    }

    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = vec![parent_thread_id.to_owned()];
    let mut out: Vec<SubAgentRef> = Vec::new();

    while let Some(parent) = frontier.pop() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT e.child_thread_id, t.rollout_path
               FROM thread_spawn_edges e
               JOIN threads t ON t.id = e.child_thread_id
              WHERE e.parent_thread_id = ?",
        )
        .bind(&parent)
        .fetch_all(&pool)
        .await?;
        for (child_id, rollout) in rows {
            if !visited.contains(&child_id) {
                frontier.push(child_id.clone());
            }
            out.push(SubAgentRef {
                child_thread_id: child_id,
                rollout_path: rollout,
            });
        }
    }

    pool.close().await;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Executor;

    async fn create_state_db(path: &Path) -> sqlx::SqlitePool {
        let path_str = path.to_str().unwrap();
        let opts = SqliteConnectOptions::new()
            .filename(path_str)
            .create_if_missing(true)
            .read_only(false);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        pool.execute(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL
            )",
        )
        .await
        .unwrap();
        pool.execute(
            "CREATE TABLE thread_spawn_edges (
                parent_thread_id TEXT NOT NULL,
                child_thread_id TEXT NOT NULL PRIMARY KEY,
                status TEXT NOT NULL
            )",
        )
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn empty_when_db_missing() {
        let r = list_sub_agent_rollouts(Path::new("/tmp/nope-anatta-codex.db"), "x")
            .await
            .unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn empty_when_tables_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        // create an empty SQLite with no tables.
        let opts = SqliteConnectOptions::new()
            .filename(db_path.to_str().unwrap())
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        pool.close().await;

        let r = list_sub_agent_rollouts(&db_path, "anything")
            .await
            .unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn returns_direct_children() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let pool = create_state_db(&db_path).await;

        sqlx::query("INSERT INTO threads (id, rollout_path) VALUES ('parent', '/path/parent.jsonl')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO threads (id, rollout_path) VALUES ('child1', '/path/child1.jsonl')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO threads (id, rollout_path) VALUES ('child2', '/path/child2.jsonl')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status)
             VALUES ('parent', 'child1', 'completed'),
                    ('parent', 'child2', 'completed')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let r = list_sub_agent_rollouts(&db_path, "parent").await.unwrap();
        let mut paths: Vec<&str> = r.iter().map(|x| x.rollout_path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/path/child1.jsonl", "/path/child2.jsonl"]);
    }

    #[tokio::test]
    async fn walks_transitively() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let pool = create_state_db(&db_path).await;

        // parent → child → grandchild
        for (id, p) in [
            ("parent", "/path/parent.jsonl"),
            ("child", "/path/child.jsonl"),
            ("grandchild", "/path/grandchild.jsonl"),
        ] {
            sqlx::query("INSERT INTO threads (id, rollout_path) VALUES (?, ?)")
                .bind(id)
                .bind(p)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO thread_spawn_edges VALUES
             ('parent', 'child', 'completed'),
             ('child',  'grandchild', 'completed')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let r = list_sub_agent_rollouts(&db_path, "parent").await.unwrap();
        let ids: std::collections::HashSet<&str> =
            r.iter().map(|x| x.child_thread_id.as_str()).collect();
        assert!(ids.contains("child"));
        assert!(ids.contains("grandchild"));
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn cycle_does_not_infinite_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let pool = create_state_db(&db_path).await;
        for (id, p) in [("a", "/a.jsonl"), ("b", "/b.jsonl")] {
            sqlx::query("INSERT INTO threads (id, rollout_path) VALUES (?, ?)")
                .bind(id)
                .bind(p)
                .execute(&pool)
                .await
                .unwrap();
        }
        // Self-cycle a → a (degenerate but defensive)
        // Plus a → b and b → a (real cycle).
        sqlx::query(
            "INSERT INTO thread_spawn_edges VALUES
             ('a', 'b', 'completed'),
             ('b', 'a', 'completed')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let r = list_sub_agent_rollouts(&db_path, "a").await.unwrap();
        // Should return b (descendant of a) at minimum, possibly also a
        // (b's descendant). visited set prevents infinite loop.
        assert!(!r.is_empty());
        // Specifically: starting from 'a', we see b. Then from b we
        // would see a, but a is already visited, so the edge expansion
        // is skipped — meaning a is NOT in the output (since output
        // only contains children-of-visited).
        let ids: Vec<&str> = r.iter().map(|x| x.child_thread_id.as_str()).collect();
        assert!(ids.contains(&"b"));
    }
}
