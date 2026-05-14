//! Claude `--print --output-format stream-json` launch.

use std::path::PathBuf;

use anatta_core::{AgentEvent, ProjectionContext, Projector};
use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;

use super::pipeline::{spawn_with_pipeline, PipelineHandles};
use super::{AgentSession, ClaudeSessionId, Launchable, SpawnError};
use crate::claude::stream::ClaudeStreamEvent;
use crate::claude::StreamProjector;
use crate::profile::ClaudeProfile;

/// Configuration for spawning a claude session.
#[derive(Debug, Clone)]
pub struct ClaudeLaunch {
    pub profile: ClaudeProfile,
    pub cwd: PathBuf,
    pub prompt: String,
    /// `Some(id)` → launch with `--resume <id>` to continue an existing
    /// session. `None` → start fresh.
    pub resume: Option<ClaudeSessionId>,
    /// Path to the claude binary. Use [`crate::distribution::install`]
    /// (with the `installer` feature) to obtain it under anatta-managed
    /// paths.
    pub binary_path: PathBuf,
    /// Provider routing. `Some(env)` injects ANTHROPIC_BASE_URL / AUTH_TOKEN /
    /// MODEL / vendor extras into the child. `None` = use claude-cli's own
    /// auth + endpoint (OAuth keychain path).
    pub provider: Option<crate::profile::ProviderEnv>,
}

#[async_trait]
impl Launchable for ClaudeLaunch {
    async fn launch(self) -> Result<AgentSession, SpawnError> {
        if !self.binary_path.exists() {
            return Err(SpawnError::BinaryNotFound(self.binary_path.clone()));
        }
        if !self.profile.path.is_dir() {
            return Err(SpawnError::ProfilePathInvalid(self.profile.path.clone()));
        }

        let mut cmd = Command::new(&self.binary_path);
        cmd.env("CLAUDE_CONFIG_DIR", &self.profile.path);
        if let Some(env) = &self.provider {
            for (k, v) in &env.vars {
                cmd.env(k, v);
            }
        }
        cmd.current_dir(&self.cwd);
        cmd.arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            // anatta orchestrates the conversation lifecycle (segment locking,
            // workspace, etc.); claude's interactive permission prompts are
            // never visible in `--print` mode and stall the turn. Bypass them.
            .arg("--dangerously-skip-permissions");
        if let Some(id) = &self.resume {
            cmd.arg("--resume").arg(id.as_str());
        }
        cmd.arg(&self.prompt);

        let mut projector = StreamProjector::new();
        let line_to_events = move |line: &str| -> Vec<AgentEvent> {
            let raw: ClaudeStreamEvent = match serde_json::from_str(line) {
                Ok(r) => r,
                // Skip malformed lines; parser fixture suite owns coverage.
                Err(_) => return Vec::new(),
            };
            let ctx = ProjectionContext {
                session_id: String::new(),
                received_at: Utc::now(),
            };
            projector.project(&raw, &ctx)
        };

        let handles: PipelineHandles = spawn_with_pipeline(cmd, line_to_events).await?;

        super::finalize_first_event_session(handles).await
    }
}
