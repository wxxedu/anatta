//! SQLite-backed persistence for anatta.
//!
//! [`Store`] owns a connection pool and surfaces typed CRUD over each
//! domain table. CLI and daemon-core both open the same DB file; SQLite
//! WAL mode handles their concurrent access (CLI writes are rare, daemon
//! reads/writes session state).
//!
//! Migrations live in `./migrations/` and are bundled at compile time
//! via [`sqlx::migrate!`]. Both CLI and daemon run them on startup;
//! `sqlx::migrate` is idempotent.

pub mod conversation;
pub mod migrate;
pub mod profile;
pub mod segment;

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Storage handle. Wrap an open `SqlitePool` and dispatch into per-table
/// query modules ([`profile`], later `intent`, `session`, ...).
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open or create the anatta DB at `<anatta_home>/anatta.db`.
    /// Runs pending migrations before returning.
    pub async fn open(anatta_home: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(anatta_home).map_err(StoreError::Io)?;
        let db_path = anatta_home.join("anatta.db");
        let url = format!("sqlite:{}", db_path.display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        migrate::run_tier3_post_migration(&pool).await?;
        Ok(Self { pool })
    }

    /// Test/in-process variant: open an ephemeral in-memory DB. Each call
    /// returns a separate DB. Useful for unit / integration tests.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        MIGRATOR.run(&pool).await?;
        migrate::run_tier3_post_migration(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("unknown backend kind: {0:?}")]
    UnknownBackend(String),
    #[error("unknown auth method: {0:?}")]
    UnknownAuthMethod(String),
    #[error("migration blocked: {0}")]
    MigrationBlocked(String),
}
