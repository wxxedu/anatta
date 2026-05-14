//! `conversations` table CRUD for `anatta chat`.
//!
//! Lock semantics moved out of the store (see migration 0005): the
//! per-conversation exclusive lock now lives in `anatta-runtime`'s
//! [`SessionLock`](anatta_runtime::SessionLock), backed by flock under
//! `<anatta_home>/runtime-locks/`. The store is back to being a pure
//! KV layer for conversation metadata.

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
}

#[derive(Debug, Clone)]
pub struct NewConversation<'a> {
    pub name: &'a str,
    pub profile_id: &'a str,
    pub cwd: &'a str,
}

struct ConversationRow {
    name: String,
    profile_id: String,
    backend_session_id: Option<String>,
    cwd: String,
    last_used_at: String,
}

impl ConversationRow {
    fn into_record(self) -> Result<ConversationRecord, StoreError> {
        Ok(ConversationRecord {
            name: self.name,
            profile_id: self.profile_id,
            backend_session_id: self.backend_session_id,
            cwd: self.cwd,
            last_used_at: parse_ts(&self.last_used_at)?,
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
                name, profile_id, backend_session_id, cwd, last_used_at
            )
            VALUES (?, ?, NULL, ?, ?)
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
                last_used_at       AS "last_used_at!"
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
    /// `anatta send --resume <id>` so it can find a matching named
    /// conversation and grab its SessionLock before talking to the
    /// backend.
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
                last_used_at       AS "last_used_at!"
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
                last_used_at       AS "last_used_at!"
            FROM conversations
            ORDER BY last_used_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ConversationRow::into_record).collect()
    }

    /// Delete a conversation row unconditionally. The caller is
    /// responsible for ensuring no one is currently holding the
    /// runtime SessionLock for this name (the CLI does so by attempting
    /// to acquire the lock before deleting, which serves as a "is
    /// anyone using this?" check).
    ///
    /// Returns true if a row was deleted.
    pub async fn delete_conversation(&self, name: &str) -> Result<bool, StoreError> {
        let res = sqlx::query!("DELETE FROM conversations WHERE name = ?", name)
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

    /// Swap the profile a conversation is associated with. Used by the
    /// in-chat `/profile` command for same-backend reconfiguration
    /// (claude→claude with different API key, codex→codex with
    /// different env). The caller is responsible for verifying the new
    /// profile's backend matches the existing conversation's backend
    /// (the store doesn't enforce it because the column isn't denormalized).
    pub async fn set_conversation_profile(
        &self,
        name: &str,
        new_profile_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE conversations SET profile_id = ? WHERE name = ?",
            new_profile_id,
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

    // ── tier 1: segments / family-aware additions ─────────────────────────

    /// Ensure this conversation has the new tier-1 columns populated
    /// (`id` ULID, `backend`, `session_uuid` if known, `created_at`).
    /// Idempotent: if `id` is already non-NULL, no-op (returns existing id).
    ///
    /// Generates a fresh ULID for `id` if missing.
    /// `backend` is sourced from the conversation's current
    /// `profile_id` join.
    /// `session_uuid` defaults to the existing `backend_session_id` if
    /// available; otherwise stays NULL.
    /// `created_at` defaults to `last_used_at`.
    ///
    /// Returns the conversation's ULID id.
    pub async fn ensure_conversation_metadata(
        &self,
        name: &str,
    ) -> Result<String, StoreError> {
        // Read current state. Tier 3 destructive drop has removed
        // `conversations.backend` and `session_uuid`; only id +
        // created_at + last_used_at remain. The backend identity now
        // lives on the segment row(s), which the orch layer sets via
        // segment.backend on insert.
        let row = sqlx::query!(
            r#"
            SELECT
                c.id,
                c.created_at,
                c.last_used_at AS "last_used_at!"
            FROM conversations c
            WHERE c.name = ?
            "#,
            name,
        )
        .fetch_one(&self.pool)
        .await?;

        if let Some(existing) = row.id {
            if !existing.is_empty() {
                return Ok(existing);
            }
        }

        let new_id = ulid::Ulid::new().to_string();
        let created_at = row.created_at.unwrap_or(row.last_used_at);

        sqlx::query!(
            r#"
            UPDATE conversations
               SET id = ?,
                   created_at = COALESCE(created_at, ?)
             WHERE name = ?
            "#,
            new_id,
            created_at,
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(new_id)
    }

    /// Read tier-3 metadata. The `backend` and `session_uuid` fields
    /// on the returned struct are always `None` after the tier-3
    /// destructive drop runs — callers should read backend and
    /// engine_session_id from the appropriate segment via
    /// `get_segment` / `list_segments` / `active_segment`.
    pub async fn get_conversation_metadata(
        &self,
        name: &str,
    ) -> Result<Option<ConversationMetadata>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id, created_at, cwd AS "cwd!"
            FROM conversations
            WHERE name = ?
            "#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(ConversationMetadata {
                id: r.id,
                backend: None,
                session_uuid: None,
                created_at: r.created_at.as_deref().map(parse_ts).transpose()?,
                cwd: r.cwd,
            })),
        }
    }

    /// Legacy: previously set both `conversations.session_uuid` and
    /// `conversations.backend_session_id`. After the tier-3
    /// destructive drop, `session_uuid` no longer exists; this writes
    /// only `backend_session_id` (kept for the `send --resume`
    /// reverse-lookup helper). Tier-3 callers should use
    /// `set_engine_session_id` on the active segment instead.
    pub async fn set_session_uuid(
        &self,
        name: &str,
        uuid: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE conversations
               SET backend_session_id = COALESCE(backend_session_id, ?)
             WHERE name = ?
            "#,
            uuid,
            name,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lookup by ULID. Used by send --resume path and crash-recovery sweeps.
    pub async fn get_conversation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<ConversationMetadata>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id, created_at, cwd AS "cwd!", name AS "name!"
            FROM conversations
            WHERE id = ?
            "#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(ConversationMetadata {
                id: r.id,
                backend: None,
                session_uuid: None,
                created_at: r.created_at.as_deref().map(parse_ts).transpose()?,
                cwd: r.cwd,
            })),
        }
    }

    /// Lookup name from id. Convenience for resolving lock keys.
    pub async fn conversation_name_by_id(
        &self,
        id: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(sqlx::query_scalar!(
            r#"SELECT name AS "name!" FROM conversations WHERE id = ?"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// List conversations that have an active segment (`ended_at IS NULL`)
    /// — used by the crash-recovery sweep on anatta startup.
    pub async fn conversations_with_active_segments(
        &self,
    ) -> Result<Vec<ConversationMetadata>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT c.id, c.created_at, c.cwd AS "cwd!", c.name AS "name!"
            FROM conversations c
            JOIN conversation_segments s ON s.conversation_id = c.id
            WHERE s.ended_at IS NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok::<_, StoreError>(ConversationMetadata {
                    id: r.id,
                    backend: None,
                    session_uuid: None,
                    created_at: r.created_at.as_deref().map(parse_ts).transpose()?,
                    cwd: r.cwd,
                })
            })
            .collect()
    }
}

/// Tier-1 metadata view (new columns added in migration 0006).
#[derive(Debug, Clone)]
pub struct ConversationMetadata {
    pub id: Option<String>,
    pub backend: Option<String>,
    pub session_uuid: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub cwd: String,
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
            family_override: None,
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
    async fn delete_returns_true_only_when_row_existed() {
        let s = store_with_profile().await;
        insert(&s, "foo").await;
        assert!(s.delete_conversation("foo").await.unwrap());
        assert!(!s.delete_conversation("foo").await.unwrap());
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
