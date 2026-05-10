//! Resolved CLI runtime config.

use std::path::PathBuf;

use anatta_store::Store;

#[derive(Debug)]
pub struct Config {
    pub anatta_home: PathBuf,
    pub store: Store,
}

impl Config {
    /// Resolve `anatta_home` from (in order): the CLI flag, the
    /// `ANATTA_HOME` env var, then `~/.anatta`. Open the SQLite store
    /// (creating + migrating if needed).
    pub async fn resolve(flag: Option<PathBuf>) -> Result<Self, ConfigError> {
        let anatta_home = if let Some(p) = flag {
            p
        } else if let Some(env) = std::env::var_os("ANATTA_HOME") {
            PathBuf::from(env)
        } else {
            let home = std::env::var_os("HOME").ok_or(ConfigError::HomeNotSet)?;
            PathBuf::from(home).join(".anatta")
        };
        let store = Store::open(&anatta_home).await?;
        Ok(Self { anatta_home, store })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("HOME env var not set; pass --anatta-home or set ANATTA_HOME")]
    HomeNotSet,
    #[error("opening anatta store: {0}")]
    Store(#[from] anatta_store::StoreError),
}
