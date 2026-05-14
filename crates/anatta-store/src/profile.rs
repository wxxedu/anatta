//! Profile table CRUD.

use chrono::{DateTime, Utc};

use crate::{Store, StoreError};

/// Backend kind as stored in the `profile.backend` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Claude,
    Codex,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
        }
    }
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "claude" => Ok(BackendKind::Claude),
            "codex" => Ok(BackendKind::Codex),
            other => Err(StoreError::UnknownBackend(other.to_owned())),
        }
    }
}

/// How the profile authenticates against its backend's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Login,
    ApiKey,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Login => "login",
            AuthMethod::ApiKey => "api_key",
        }
    }
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "login" => Ok(AuthMethod::Login),
            "api_key" => Ok(AuthMethod::ApiKey),
            other => Err(StoreError::UnknownAuthMethod(other.to_owned())),
        }
    }
}

/// Public typed view of one row in the `profile` table.
#[derive(Debug, Clone)]
pub struct ProfileRecord {
    pub id: String,
    pub backend: BackendKind,
    pub name: String,
    pub auth_method: AuthMethod,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,

    pub provider: String,
    pub base_url_override: Option<String>,
    pub model_override: Option<String>,
    pub small_fast_model_override: Option<String>,
    pub default_opus_model_override: Option<String>,
    pub default_sonnet_model_override: Option<String>,
    pub default_haiku_model_override: Option<String>,
    pub subagent_model_override: Option<String>,

    /// Family override (NULL = derive from (backend, provider)). Added
    /// in migration 0006. Valid: 'a-native'|'a-compat'|'o-native'|'o-compat'.
    pub family_override: Option<String>,
}

/// What the caller passes to insert a new row.
#[derive(Debug, Clone)]
pub struct NewProfile<'a> {
    pub id: &'a str,
    pub backend: BackendKind,
    pub name: &'a str,
    pub auth_method: AuthMethod,

    pub provider: &'a str,
    pub base_url_override: Option<&'a str>,
    pub model_override: Option<&'a str>,
    pub small_fast_model_override: Option<&'a str>,
    pub default_opus_model_override: Option<&'a str>,
    pub default_sonnet_model_override: Option<&'a str>,
    pub default_haiku_model_override: Option<&'a str>,
    pub subagent_model_override: Option<&'a str>,
    pub family_override: Option<&'a str>,
}

/// Internal flat row, populated directly by `sqlx::query_as!`.
struct ProfileRow {
    id: String,
    backend: String,
    name: String,
    auth_method: String,
    created_at: String,
    last_used_at: Option<String>,
    provider: String,
    base_url_override: Option<String>,
    model_override: Option<String>,
    small_fast_model_override: Option<String>,
    default_opus_model_override: Option<String>,
    default_sonnet_model_override: Option<String>,
    default_haiku_model_override: Option<String>,
    subagent_model_override: Option<String>,
    family_override: Option<String>,
}

impl ProfileRow {
    fn into_record(self) -> Result<ProfileRecord, StoreError> {
        Ok(ProfileRecord {
            backend: BackendKind::parse(&self.backend)?,
            auth_method: AuthMethod::parse(&self.auth_method)?,
            created_at: parse_ts(&self.created_at)?,
            last_used_at: self.last_used_at.map(|s| parse_ts(&s)).transpose()?,
            id: self.id,
            name: self.name,
            provider: self.provider,
            base_url_override: self.base_url_override,
            model_override: self.model_override,
            small_fast_model_override: self.small_fast_model_override,
            default_opus_model_override: self.default_opus_model_override,
            default_sonnet_model_override: self.default_sonnet_model_override,
            default_haiku_model_override: self.default_haiku_model_override,
            subagent_model_override: self.subagent_model_override,
            family_override: self.family_override,
        })
    }
}

impl Store {
    /// Insert a new profile. Fails on `(backend, name)` collision.
    pub async fn insert_profile(&self, p: NewProfile<'_>) -> Result<(), StoreError> {
        let backend = p.backend.as_str();
        let auth = p.auth_method.as_str();
        let now = Utc::now().to_rfc3339();
        sqlx::query!(
            r#"
            INSERT INTO profile (
                id, backend, name, auth_method, created_at, last_used_at,
                provider,
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override,
                family_override
            )
            VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            p.id,
            backend,
            p.name,
            auth,
            now,
            p.provider,
            p.base_url_override,
            p.model_override,
            p.small_fast_model_override,
            p.default_opus_model_override,
            p.default_sonnet_model_override,
            p.default_haiku_model_override,
            p.subagent_model_override,
            p.family_override,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all profiles, most-recently-used first
    /// (NULL last_used_at falls back to created_at).
    pub async fn list_profiles(&self) -> Result<Vec<ProfileRecord>, StoreError> {
        let rows = sqlx::query_as!(
            ProfileRow,
            r#"
            SELECT
                id           AS "id!",
                backend      AS "backend!",
                name         AS "name!",
                auth_method  AS "auth_method!",
                created_at   AS "created_at!",
                last_used_at,
                provider     AS "provider!",
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override,
                family_override
            FROM profile
            ORDER BY COALESCE(last_used_at, created_at) DESC
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ProfileRow::into_record).collect()
    }

    pub async fn get_profile(&self, id: &str) -> Result<Option<ProfileRecord>, StoreError> {
        let row = sqlx::query_as!(
            ProfileRow,
            r#"
            SELECT
                id           AS "id!",
                backend      AS "backend!",
                name         AS "name!",
                auth_method  AS "auth_method!",
                created_at   AS "created_at!",
                last_used_at,
                provider     AS "provider!",
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override,
                family_override
            FROM profile
            WHERE id = ?
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(ProfileRow::into_record).transpose()
    }

    /// Returns true if a row was deleted.
    pub async fn delete_profile(&self, id: &str) -> Result<bool, StoreError> {
        let res = sqlx::query!("DELETE FROM profile WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Stamp `last_used_at = now()`. Daemon calls this when a session
    /// launches against this profile.
    pub async fn touch_profile(&self, id: &str) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query!("UPDATE profile SET last_used_at = ? WHERE id = ?", now, id)
            .execute(&self.pool)
            .await?;
        Ok(())
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

    async fn store() -> Store {
        Store::open_in_memory().await.expect("in-memory store")
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-AbCd1234",
            backend: BackendKind::Claude,
            name: "work",
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

        let got = s.get_profile("claude-AbCd1234").await.unwrap().unwrap();
        assert_eq!(got.id, "claude-AbCd1234");
        assert_eq!(got.backend, BackendKind::Claude);
        assert_eq!(got.name, "work");
        assert_eq!(got.auth_method, AuthMethod::Login);
        assert_eq!(got.provider, "anthropic");
        assert!(got.last_used_at.is_none());
    }

    #[tokio::test]
    async fn list_orders_by_recency() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-A",
            backend: BackendKind::Claude,
            name: "first",
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
        // Make second insertion strictly later in created_at terms
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        s.insert_profile(NewProfile {
            id: "codex-B",
            backend: BackendKind::Codex,
            name: "second",
            auth_method: AuthMethod::ApiKey,
            provider: "openai",
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

        let list = s.list_profiles().await.unwrap();
        assert_eq!(list.len(), 2);
        // Newer row first.
        assert_eq!(list[0].id, "codex-B");
        assert_eq!(list[1].id, "claude-A");

        // Touching the older row promotes it.
        s.touch_profile("claude-A").await.unwrap();
        let list = s.list_profiles().await.unwrap();
        assert_eq!(list[0].id, "claude-A");
    }

    #[tokio::test]
    async fn unique_constraint_on_backend_name() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-X",
            backend: BackendKind::Claude,
            name: "dup",
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
        let err = s
            .insert_profile(NewProfile {
                id: "claude-Y",
                backend: BackendKind::Claude,
                name: "dup",
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
            .unwrap_err();
        assert!(matches!(err, StoreError::Sqlx(_)));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_row_existed() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-DEL",
            backend: BackendKind::Claude,
            name: "delete-me",
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
        assert!(s.delete_profile("claude-DEL").await.unwrap());
        assert!(!s.delete_profile("claude-DEL").await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let s = store().await;
        assert!(s.get_profile("never-was").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn round_trip_with_provider_and_overrides() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-DSeek01",
            backend: BackendKind::Claude,
            name: "ds",
            auth_method: AuthMethod::ApiKey,
            provider: "deepseek",
            base_url_override: None,
            model_override: Some("deepseek-v4-pro"),
            small_fast_model_override: None,
            default_opus_model_override: None,
            default_sonnet_model_override: None,
            default_haiku_model_override: Some("deepseek-v4-flash"),
            subagent_model_override: None,
            family_override: None,
        })
        .await
        .unwrap();

        let got = s.get_profile("claude-DSeek01").await.unwrap().unwrap();
        assert_eq!(got.provider, "deepseek");
        assert_eq!(got.model_override.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(
            got.default_haiku_model_override.as_deref(),
            Some("deepseek-v4-flash")
        );
        assert!(got.base_url_override.is_none());
        assert!(got.subagent_model_override.is_none());
    }

    #[tokio::test]
    async fn legacy_default_provider_for_inserts() {
        // Mimic old code path: API forces caller to supply provider, so
        // there is no default-from-DB path tested via Rust API. Instead
        // we just confirm a claude profile can be inserted with provider=anthropic.
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-Anth001",
            backend: BackendKind::Claude,
            name: "default",
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
        let got = s.get_profile("claude-Anth001").await.unwrap().unwrap();
        assert_eq!(got.provider, "anthropic");
    }
}
