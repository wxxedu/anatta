//! `anatta send` — placeholder stub (not yet implemented).

use clap::Args;

use crate::config::Config;

#[derive(Args, Debug)]
pub struct SendArgs {
    /// The prompt text to send.
    prompt: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("send is not yet implemented")]
    NotImplemented,
}

pub async fn run(_args: SendArgs, _cfg: &Config) -> Result<(), SendError> {
    Err(SendError::NotImplemented)
}
