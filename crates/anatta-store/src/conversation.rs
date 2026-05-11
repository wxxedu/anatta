//! `conversations` table CRUD + lock SQL for `anatta chat`.
//!
//! The lock is per-name and prevents two `anatta chat` processes from
//! spawning the backend (claude/codex) against the same on-disk session
//! file at the same time. Acquire is a single SQLite `BEGIN IMMEDIATE`
//! transaction that:
//!
//!   1. SELECTs the current `(lock_holder_pid, lock_holder_started_at)`,
//!   2. Asks a caller-provided closure whether that holder is still
//!      the same live process,
//!   3. If NULL or dead/reused → UPDATEs to `(my_pid, my_started_at)`,
//!   4. Else → reports `Held { pid }`.
//!
//! Liveness is a CLI-side concern (sysinfo); this crate accepts a
//! `FnOnce` so it doesn't grow a process-introspection dependency.

use chrono::{DateTime, Utc};

use crate::{Store, StoreError};

/// Public typed view of one row in the `conversations` table.
#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub name: String,
    pub profile_id: String,
    pub backend_session_id: Option<String>,
    pub cwd: String,
    pub last_used_at: DateTime<Utc>,
    pub lock_holder_pid: Option<i64>,
    pub lock_holder_started_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewConversation<'a> {
    pub name: &'a str,
    pub profile_id: &'a str,
    pub cwd: &'a str,
}

/// Outcome of `try_acquire_with_check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// Lock now held by the caller's pid.
    Acquired,
    /// Held by another live process.
    Held { pid: i64 },
}

/// Internal flat row for sqlx::query_as!.
struct ConversationRow {
    name: String,
    profile_id: String,
    backend_session_id: Option<String>,
    cwd: String,
    last_used_at: String,
    lock_holder_pid: Option<i64>,
    lock_holder_started_at: Option<i64>,
}

impl ConversationRow {
    fn into_record(self) -> Result<ConversationRecord, StoreError> {
        Ok(ConversationRecord {
            name: self.name,
            profile_id: self.profile_id,
            backend_session_id: self.backend_session_id,
            cwd: self.cwd,
            last_used_at: parse_ts(&self.last_used_at)?,
            lock_holder_pid: self.lock_holder_pid,
            lock_holder_started_at: self.lock_holder_started_at,
        })
    }
}

impl Store {
    /// Insert a new conversation. Fails on `name` collision (PK).
    pub async fn insert_conversation(&self, c: NewConversation<'_>) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query!(
            r#"
            INSERT INTO conversations (
                name, profile_id, backend_session_id, cwd, last_used_at, lock_holder_pid
            )
            VALUES (?, ?, NULL, ?, ?, NULL)
            "#,
            c.name,
            c.profile_id,
            c.cwd,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_conversation(
        &self,
        name: &str,
    ) -> Result<Option<ConversationRecord>, StoreError> {
        let row = sqlx::query_as!(
            ConversationRow,
            r#"
            SELECT
                name               AS "name!",
                profile_id         AS "profile_id!",
                backend_session_id,
                cwd                AS "cwd!",
                last_used_at       AS "last_used_at!",
                lock_holder_pid,
                lock_holder_started_at
            FROM conversations
            WHERE name = ?
            "#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(ConversationRow::into_record).transpose()
    }

    /// Reverse-lookup a conversation by its backend_session_id. Used by
    /// `anatta send --resume <id>` to optionally pick up the chat lock
    /// when the id refers to a named conversation.
    pub async fn get_conversation_by_backend_session_id(
        &self,
        backend_session_id: &str,
    ) -> Result<Option<ConversationRecord>, StoreError> {
        let row = sqlx::query_as!(
            ConversationRow,
            r#"
            SELECT
                name               AS "name!",
                profile_id         AS "profile_id!",
                backend_session_id,
                cwd                AS "cwd!",
                last_used_at       AS "last_used_at!",
                lock_holder_pid,
                lock_holder_started_at
            FROM conversations
            WHERE backend_session_id = ?
            "#,
            backend_session_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(ConversationRow::into_record).transpose()
    }

    /// List all conversations, most-recently-used first.
    pub async fn list_conversations(&self) -> Result<Vec<ConversationRecord>, StoreError> {
        let rows = sqlx::query_as!(
            ConversationRow,
            r#"
            SELECT
                name               AS "name!",
                profile_id         AS "profile_id!",
                backend_session_id,
                cwd                AS "cwd!",
                last_used_at       AS "last_used_at!",
                lock_holder_pid,
                lock_holder_started_at
            FROM conversations
            ORDER BY last_used_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ConversationRow::into_record).collect()
    }

    /// Delete a conversation. Refuses if locked. Returns true if a row was deleted.
    pub async fn delete_conversation(&self, name: &str) -> Result<bool, StoreError> {
        let res = sqlx::query!(
            "DELETE FROM conversations WHERE name = ? AND lock_holder_pid IS NULL",
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Stamp last_used_at = now. Called per turn.
    pub async fn touch_conversation(&self, name: &str) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query!(
            "UPDATE conversations SET last_used_at = ? WHERE name = ?",
            now,
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set the backend session id (claude session UUID / codex thread UUID).
    /// No-op if already non-NULL — the id is immutable across the lifetime
    /// of the conversation.
    pub async fn set_backend_session_id(
        &self,
        name: &str,
        backend_session_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE conversations
            SET backend_session_id = ?
            WHERE name = ? AND backend_session_id IS NULL
            "#,
            backend_session_id,
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically acquire the per-name lock for `my_pid` / `my_started_at`.
    ///
    /// Uses `BEGIN IMMEDIATE` so the SELECT-then-UPDATE pair is
    /// serialized against concurrent writers. The caller-provided
    /// `is_same_alive` closure receives the existing holder's
    /// `(pid, started_at)` and decides whether that exact process is
    /// still alive (different start time = PID was reused = treat as stale).
    pub async fn try_acquire_with_check<F>(
        &self,
        name: &str,
        my_pid: i64,
        my_started_at: i64,
        is_same_alive: F,
    ) -> Result<AcquireOutcome, StoreError>
    where
        F: FnOnce(i64, Option<i64>) -> bool,
    {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let row = sqlx::query!(
            r#"
            SELECT lock_holder_pid, lock_holder_started_at
            FROM conversations
            WHERE name = ?
            "#,
            name,
        )
        .fetch_optional(&mut *conn)
        .await?;

        let row = match row {
            Some(r) => r,
            None => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await?;
                return Err(StoreError::ConversationNotFound(name.to_owned()));
            }
        };

        let can_take = match row.lock_holder_pid {
            None => true,
            Some(pid) => !is_same_alive(pid, row.lock_holder_started_at),
        };
        if !can_take {
            sqlx::query("ROLLBACK").execute(&mut *conn).await?;
            return Ok(AcquireOutcome::Held {
                pid: row.lock_holder_pid.unwrap(),
            });
        }

        sqlx::query!(
            r#"
            UPDATE conversations
            SET lock_holder_pid = ?, lock_holder_started_at = ?
            WHERE name = ?
            "#,
            my_pid,
            my_started_at,
            name,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(AcquireOutcome::Acquired)
    }

    /// Release the lock if and only if the caller is still the recorded
    /// holder. PID alone identifies the row since we wrote both pid +
    /// started_at atomically; releasing also clears the started_at column.
    pub async fn release_lock_if_held(
        &self,
        name: &str,
        my_pid: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE conversations
            SET lock_holder_pid = NULL, lock_holder_started_at = NULL
            WHERE name = ? AND lock_holder_pid = ?
            "#,
            name,
            my_pid,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Manual escape hatch: `anatta chat unlock <name>`.
    /// Clears the lock unconditionally.
    pub async fn force_unlock(&self, name: &str) -> Result<bool, StoreError> {
        let res = sqlx::query!(
            r#"
            UPDATE conversations
            SET lock_holder_pid = NULL, lock_holder_started_at = NULL
            WHERE name = ?
            "#,
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| StoreError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{AuthMethod, BackendKind, NewProfile};

    async fn store_with_profile() -> Store {
        let s = Store::open_in_memory().await.unwrap();
        s.insert_profile(NewProfile {
            id: "claude-AbCd1234",
            backend: BackendKind::Claude,
            name: "test",
            auth_method: AuthMethod::Login,
            provider: "anthropic",
            base_url_override: None,
            model_override: None,
            small_fast_model_override: None,
            default_opus_model_override: None,
            default_sonnet_model_override: None,
            default_haiku_model_override: None,
            subagent_model_override: None,
        })
        .await
        .unwrap();
        s
    }

    async fn insert(s: &Store, name: &str) {
        s.insert_conversation(NewConversation {
            name,
            profile_id: "claude-AbCd1234",
            cwd: "/tmp/test",
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        let got = s.get_conversation("foo").await.unwrap().unwrap();
        assert_eq!(got.name, "foo");
        assert_eq!(got.profile_id, "claude-AbCd1234");
        assert!(got.backend_session_id.is_none());
        assert_eq!(got.cwd, "/tmp/test");
        assert!(got.lock_holder_pid.is_none());
    }

    #[tokio::test]
    async fn set_backend_session_id_writes_once() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.set_backend_session_id("foo", "sess-1").await.unwrap();
        assert_eq!(
            s.get_conversation("foo")
                .await
                .unwrap()
                .unwrap()
                .backend_session_id
                .as_deref(),
            Some("sess-1"),
        );
        // Second call is a no-op (only updates when NULL).
        s.set_backend_session_id("foo", "sess-2").await.unwrap();
        assert_eq!(
            s.get_conversation("foo")
                .await
                .unwrap()
                .unwrap()
                .backend_session_id
                .as_deref(),
            Some("sess-1"),
        );
    }

    #[tokio::test]
    async fn acquire_then_release_round_trip() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;

        let r1 = s
            .try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        assert_eq!(r1, AcquireOutcome::Acquired);

        let r2 = s
            .try_acquire_with_check("foo", 200, 8888, |_, _| true)
            .await
            .unwrap();
        assert_eq!(r2, AcquireOutcome::Held { pid: 100 });

        s.release_lock_if_held("foo", 100).await.unwrap();
        let r3 = s
            .try_acquire_with_check("foo", 200, 8888, |_, _| true)
            .await
            .unwrap();
        assert_eq!(r3, AcquireOutcome::Acquired);
    }

    #[tokio::test]
    async fn dead_pid_is_reclaimable() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;

        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        // Original holder appears dead → next acquire wins.
        let r = s
            .try_acquire_with_check("foo", 200, 8888, |_, _| false)
            .await
            .unwrap();
        assert_eq!(r, AcquireOutcome::Acquired);
        let row = s.get_conversation("foo").await.unwrap().unwrap();
        assert_eq!(row.lock_holder_pid, Some(200));
        assert_eq!(row.lock_holder_started_at, Some(8888));
    }

    #[tokio::test]
    async fn pid_reused_treated_as_stale() {
        // Holder pid 100 was acquired at started_at 7777. Later the OS
        // reuses pid 100 for a different process at started_at 9999.
        // The is_same_alive callback sees (pid=100, recorded=Some(7777))
        // and returns false because the current start time differs.
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        let r = s
            .try_acquire_with_check("foo", 200, 8888, |pid, recorded| {
                assert_eq!(pid, 100);
                assert_eq!(recorded, Some(7777));
                false
            })
            .await
            .unwrap();
        assert_eq!(r, AcquireOutcome::Acquired);
    }

    #[tokio::test]
    async fn release_only_clears_for_matching_pid() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        // Wrong PID → no-op.
        s.release_lock_if_held("foo", 999).await.unwrap();
        assert_eq!(
            s.get_conversation("foo")
                .await
                .unwrap()
                .unwrap()
                .lock_holder_pid,
            Some(100),
        );
    }

    #[tokio::test]
    async fn delete_refuses_when_locked() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        assert!(!s.delete_conversation("foo").await.unwrap());
        assert!(s.get_conversation("foo").await.unwrap().is_some());

        s.release_lock_if_held("foo", 100).await.unwrap();
        assert!(s.delete_conversation("foo").await.unwrap());
        assert!(s.get_conversation("foo").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn force_unlock_clears_unconditionally() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        assert!(s.force_unlock("foo").await.unwrap());
        assert!(s
            .get_conversation("foo")
            .await
            .unwrap()
            .unwrap()
            .lock_holder_pid
            .is_none());
    }

    #[tokio::test]
    async fn list_orders_by_recency() {
        let s = store_with_profile().await;
        insert(&s, "old").await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        insert(&s, "new").await;

        let list = s.list_conversations().await.unwrap();
        assert_eq!(list[0].name, "new");
        assert_eq!(list[1].name, "old");

        s.touch_conversation("old").await.unwrap();
        let list = s.list_conversations().await.unwrap();
        assert_eq!(list[0].name, "old");
    }

    #[tokio::test]
    async fn acquire_against_missing_conversation_errors() {
        let s = store_with_profile().await;
        let err = s
            .try_acquire_with_check("ghost", 100, 7777, |_, _| true)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ConversationNotFound(_)));
    }

    #[tokio::test]
    async fn release_clears_both_pid_and_started_at() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        s.release_lock_if_held("foo", 100).await.unwrap();
        let row = s.get_conversation("foo").await.unwrap().unwrap();
        assert!(row.lock_holder_pid.is_none());
        assert!(row.lock_holder_started_at.is_none());
    }

    #[tokio::test]
    async fn force_unlock_clears_both_columns() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        s.try_acquire_with_check("foo", 100, 7777, |_, _| true)
            .await
            .unwrap();
        s.force_unlock("foo").await.unwrap();
        let row = s.get_conversation("foo").await.unwrap().unwrap();
        assert!(row.lock_holder_pid.is_none());
        assert!(row.lock_holder_started_at.is_none());
    }

    #[tokio::test]
    async fn lookup_by_backend_session_id() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        assert!(s
            .get_conversation_by_backend_session_id("sess-1")
            .await
            .unwrap()
            .is_none());
        s.set_backend_session_id("foo", "sess-1").await.unwrap();
        let row = s
            .get_conversation_by_backend_session_id("sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.name, "foo");
    }
}
