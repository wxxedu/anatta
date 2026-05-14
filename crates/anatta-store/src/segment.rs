//! `conversation_segments` table CRUD.
//!
//! A conversation has an ordered list of segments; one is "active"
//! (`ended_at IS NULL`) at any time. Profile swap closes the current
//! segment and opens a new one with the new profile.
//!
//! Tier 1 (this migration introduces both the table and these methods).

use chrono::{DateTime, Utc};

use crate::{Store, StoreError};

/// Public typed view of one segment row.
#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub id: String,
    pub conversation_id: String,
    pub ordinal: i64,
    pub profile_id: String,
    pub source_family: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub transition_policy: String,
    pub ended_with_compact: bool,
    pub last_absorbed_bytes: i64,
    pub render_initial_bytes: i64,
    /// Tier 3: which engine produced this segment ('claude' | 'codex').
    /// Mirrors the profile.backend at segment-creation time. Frozen.
    pub backend: String,
    /// Tier 3: source-engine-native session id (claude sessionId / codex
    /// thread_id). NULL until the segment's first turn produces one.
    pub engine_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewSegment<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub ordinal: i64,
    pub profile_id: &'a str,
    pub source_family: &'a str,
    /// JSON-encoded SegmentRenderPolicy.
    pub transition_policy: &'a str,
    /// Tier 3: 'claude' | 'codex'. Required.
    pub backend: &'a str,
    /// Tier 3: source-engine-native session id, if already known
    /// (e.g., resuming an existing engine session into a new
    /// conversation row). Usually NULL at insert time.
    pub engine_session_id: Option<&'a str>,
}

struct SegmentRow {
    id: String,
    conversation_id: String,
    ordinal: i64,
    profile_id: String,
    source_family: String,
    started_at: String,
    ended_at: Option<String>,
    transition_policy: String,
    ended_with_compact: i64,
    last_absorbed_bytes: i64,
    render_initial_bytes: i64,
    backend: String,
    engine_session_id: Option<String>,
}

impl SegmentRow {
    fn into_record(self) -> Result<SegmentRecord, StoreError> {
        Ok(SegmentRecord {
            id: self.id,
            conversation_id: self.conversation_id,
            ordinal: self.ordinal,
            profile_id: self.profile_id,
            source_family: self.source_family,
            started_at: parse_ts(&self.started_at)?,
            ended_at: self.ended_at.map(|s| parse_ts(&s)).transpose()?,
            transition_policy: self.transition_policy,
            ended_with_compact: self.ended_with_compact != 0,
            last_absorbed_bytes: self.last_absorbed_bytes,
            render_initial_bytes: self.render_initial_bytes,
            backend: self.backend,
            engine_session_id: self.engine_session_id,
        })
    }
}

impl Store {
    /// Insert a new segment. Caller is responsible for closing the
    /// previous active segment first (single-active-segment invariant
    /// is enforced by the partial unique index).
    pub async fn insert_segment(&self, s: NewSegment<'_>) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query!(
            r#"
            INSERT INTO conversation_segments (
                id, conversation_id, ordinal, profile_id, source_family,
                started_at, ended_at, transition_policy, ended_with_compact,
                last_absorbed_bytes, render_initial_bytes,
                backend, engine_session_id
            )
            VALUES (?, ?, ?, ?, ?, ?, NULL, ?, 0, 0, 0, ?, ?)
            "#,
            s.id,
            s.conversation_id,
            s.ordinal,
            s.profile_id,
            s.source_family,
            now,
            s.transition_policy,
            s.backend,
            s.engine_session_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the engine-native session id for a segment. Called when
    /// the first turn produces an id (claude after `system/init`, codex
    /// after `thread/start`).
    pub async fn set_engine_session_id(
        &self,
        segment_id: &str,
        engine_session_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE conversation_segments
               SET engine_session_id = ?
             WHERE id = ?
            "#,
            engine_session_id,
            segment_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch one segment row by id.
    pub async fn get_segment(&self, segment_id: &str) -> Result<Option<SegmentRecord>, StoreError> {
        let row = sqlx::query_as!(
            SegmentRow,
            r#"
            SELECT
                id                   AS "id!",
                conversation_id      AS "conversation_id!",
                ordinal              AS "ordinal!",
                profile_id           AS "profile_id!",
                source_family        AS "source_family!",
                started_at           AS "started_at!",
                ended_at,
                transition_policy    AS "transition_policy!",
                ended_with_compact   AS "ended_with_compact!",
                last_absorbed_bytes  AS "last_absorbed_bytes!",
                render_initial_bytes AS "render_initial_bytes!",
                backend              AS "backend!",
                engine_session_id
            FROM conversation_segments
            WHERE id = ?
            "#,
            segment_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(SegmentRow::into_record).transpose()
    }

    /// All segments of a conversation, in ordinal order.
    pub async fn list_segments(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<SegmentRecord>, StoreError> {
        let rows = sqlx::query_as!(
            SegmentRow,
            r#"
            SELECT
                id                   AS "id!",
                conversation_id      AS "conversation_id!",
                ordinal              AS "ordinal!",
                profile_id           AS "profile_id!",
                source_family        AS "source_family!",
                started_at           AS "started_at!",
                ended_at,
                transition_policy    AS "transition_policy!",
                ended_with_compact   AS "ended_with_compact!",
                last_absorbed_bytes  AS "last_absorbed_bytes!",
                render_initial_bytes AS "render_initial_bytes!",
                backend              AS "backend!",
                engine_session_id
            FROM conversation_segments
            WHERE conversation_id = ?
            ORDER BY ordinal ASC
            "#,
            conversation_id,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(SegmentRow::into_record).collect()
    }

    /// The active (unclosed) segment of a conversation, if any.
    pub async fn active_segment(
        &self,
        conversation_id: &str,
    ) -> Result<Option<SegmentRecord>, StoreError> {
        let row = sqlx::query_as!(
            SegmentRow,
            r#"
            SELECT
                id                   AS "id!",
                conversation_id      AS "conversation_id!",
                ordinal              AS "ordinal!",
                profile_id           AS "profile_id!",
                source_family        AS "source_family!",
                started_at           AS "started_at!",
                ended_at,
                transition_policy    AS "transition_policy!",
                ended_with_compact   AS "ended_with_compact!",
                last_absorbed_bytes  AS "last_absorbed_bytes!",
                render_initial_bytes AS "render_initial_bytes!",
                backend              AS "backend!",
                engine_session_id
            FROM conversation_segments
            WHERE conversation_id = ? AND ended_at IS NULL
            "#,
            conversation_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(SegmentRow::into_record).transpose()
    }

    /// Close a segment by setting its `ended_at` to now and optionally
    /// flagging `ended_with_compact`.
    pub async fn close_segment(
        &self,
        segment_id: &str,
        ended_with_compact: bool,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let flag: i64 = if ended_with_compact { 1 } else { 0 };
        sqlx::query!(
            r#"
            UPDATE conversation_segments
               SET ended_at = ?, ended_with_compact = ?
             WHERE id = ?
            "#,
            now,
            flag,
            segment_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update both offset fields. Render sets both equal to the freshly-
    /// rendered file size; absorb advances only `last_absorbed_bytes`.
    /// For absorb's case, pass `render_initial_bytes = None`.
    pub async fn set_segment_offsets(
        &self,
        segment_id: &str,
        last_absorbed_bytes: i64,
        render_initial_bytes: Option<i64>,
    ) -> Result<(), StoreError> {
        match render_initial_bytes {
            Some(initial) => {
                sqlx::query!(
                    r#"
                    UPDATE conversation_segments
                       SET last_absorbed_bytes = ?, render_initial_bytes = ?
                     WHERE id = ?
                    "#,
                    last_absorbed_bytes,
                    initial,
                    segment_id,
                )
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query!(
                    r#"
                    UPDATE conversation_segments
                       SET last_absorbed_bytes = ?
                     WHERE id = ?
                    "#,
                    last_absorbed_bytes,
                    segment_id,
                )
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Reset offsets to 0 (used at session end when the working file is
    /// being deleted — next session re-renders from scratch).
    pub async fn reset_segment_offsets(&self, segment_id: &str) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            UPDATE conversation_segments
               SET last_absorbed_bytes = 0, render_initial_bytes = 0
             WHERE id = ?
            "#,
            segment_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| StoreError::UnknownBackend(format!("bad timestamp: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{AuthMethod, BackendKind, NewProfile};

    async fn fresh_store() -> Store {
        Store::open_in_memory().await.unwrap()
    }

    async fn insert_profile(store: &Store, id: &str) {
        store
            .insert_profile(NewProfile {
                id,
                backend: BackendKind::Claude,
                name: id,
                auth_method: AuthMethod::ApiKey,
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
    }

    #[tokio::test]
    async fn insert_and_list() {
        let store = fresh_store().await;
        insert_profile(&store, "claude-AAA").await;

        store
            .insert_segment(NewSegment {
                id: "seg-1",
                conversation_id: "conv-1",
                ordinal: 0,
                profile_id: "claude-AAA",
                source_family: "a-native",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();

        let list = store.list_segments("conv-1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "seg-1");
        assert_eq!(list[0].source_family, "a-native");
        assert_eq!(list[0].last_absorbed_bytes, 0);
    }

    #[tokio::test]
    async fn active_segment_finds_unclosed() {
        let store = fresh_store().await;
        insert_profile(&store, "claude-AAA").await;
        store
            .insert_segment(NewSegment {
                id: "seg-0",
                conversation_id: "conv-1",
                ordinal: 0,
                profile_id: "claude-AAA",
                source_family: "a-native",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();

        let active = store.active_segment("conv-1").await.unwrap().unwrap();
        assert_eq!(active.id, "seg-0");

        store.close_segment("seg-0", false).await.unwrap();
        let active = store.active_segment("conv-1").await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn only_one_active_segment_per_conversation() {
        let store = fresh_store().await;
        insert_profile(&store, "claude-AAA").await;
        insert_profile(&store, "claude-BBB").await;

        store
            .insert_segment(NewSegment {
                id: "seg-0",
                conversation_id: "conv-1",
                ordinal: 0,
                profile_id: "claude-AAA",
                source_family: "a-native",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();

        // Trying to insert a second active segment should fail.
        let err = store
            .insert_segment(NewSegment {
                id: "seg-1",
                conversation_id: "conv-1",
                ordinal: 1,
                profile_id: "claude-BBB",
                source_family: "a-compat",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await;
        assert!(err.is_err(), "partial unique index should block two active segments");
    }

    #[tokio::test]
    async fn close_then_open_new_segment() {
        let store = fresh_store().await;
        insert_profile(&store, "claude-AAA").await;
        insert_profile(&store, "claude-BBB").await;

        store
            .insert_segment(NewSegment {
                id: "seg-0",
                conversation_id: "conv-1",
                ordinal: 0,
                profile_id: "claude-AAA",
                source_family: "a-native",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();
        store.close_segment("seg-0", false).await.unwrap();
        store
            .insert_segment(NewSegment {
                id: "seg-1",
                conversation_id: "conv-1",
                ordinal: 1,
                profile_id: "claude-BBB",
                source_family: "a-compat",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();

        let list = store.list_segments("conv-1").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].ordinal, 0);
        assert_eq!(list[1].ordinal, 1);
        assert!(list[0].ended_at.is_some());
        assert!(list[1].ended_at.is_none());
    }

    #[tokio::test]
    async fn offset_management() {
        let store = fresh_store().await;
        insert_profile(&store, "claude-AAA").await;
        store
            .insert_segment(NewSegment {
                id: "seg-0",
                conversation_id: "conv-1",
                ordinal: 0,
                profile_id: "claude-AAA",
                source_family: "a-native",
                transition_policy: r#"{"kind":"verbatim"}"#,
                backend: "claude",
                engine_session_id: None,
            })
            .await
            .unwrap();

        // Render sets both
        store.set_segment_offsets("seg-0", 100, Some(100)).await.unwrap();
        let seg = store.active_segment("conv-1").await.unwrap().unwrap();
        assert_eq!(seg.last_absorbed_bytes, 100);
        assert_eq!(seg.render_initial_bytes, 100);

        // Absorb advances only last_absorbed
        store.set_segment_offsets("seg-0", 150, None).await.unwrap();
        let seg = store.active_segment("conv-1").await.unwrap().unwrap();
        assert_eq!(seg.last_absorbed_bytes, 150);
        assert_eq!(seg.render_initial_bytes, 100);

        // Reset
        store.reset_segment_offsets("seg-0").await.unwrap();
        let seg = store.active_segment("conv-1").await.unwrap().unwrap();
        assert_eq!(seg.last_absorbed_bytes, 0);
        assert_eq!(seg.render_initial_bytes, 0);
    }
}
