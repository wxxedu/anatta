//! Codex `exec --json` launch.

use std::path::PathBuf;

use anatta_core::{AgentEvent, ProjectionContext, Projector};
use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;

use super::pipeline::{spawn_with_pipeline, PipelineHandles};
use super::{AgentSession, CodexThreadId, Launchable, SpawnError};
use crate::codex::stream::CodexStreamEvent;
use crate::codex::StreamProjector;
use crate::profile::CodexProfile;

/// Configuration for spawning a codex session.
#[derive(Debug, Clone)]
pub struct CodexLaunch {
    pub profile: CodexProfile,
    pub cwd: PathBuf,
    pub prompt: String,
    /// `Some(id)` → launch with `exec resume <id>`. `None` → fresh `exec`.
    pub resume: Option<CodexThreadId>,
    pub binary_path: PathBuf,
    /// `Some(key)` → set `OPENAI_API_KEY` on the spawned process.
    /// `None` → fall back to whatever auth codex finds in `CODEX_HOME`
    /// (`auth.json` written by a prior `codex login`).
    pub api_key: Option<String>,
}

#[async_trait]
impl Launchable for CodexLaunch {
    async fn launch(self) -> Result<AgentSession, SpawnError> {
        if !self.binary_path.exists() {
            return Err(SpawnError::BinaryNotFound(self.binary_path.clone()));
        }
        if !self.profile.path.is_dir() {
            return Err(SpawnError::ProfilePathInvalid(self.profile.path.clone()));
        }

        let mut cmd = Command::new(&self.binary_path);
        cmd.env("CODEX_HOME", &self.profile.path);
        if let Some(key) = &self.api_key {
            cmd.env("OPENAI_API_KEY", key);
        }
        cmd.current_dir(&self.cwd);
        cmd.arg("exec");
        if let Some(id) = &self.resume {
            cmd.arg("resume").arg(id.as_str());
        }
        cmd.arg("--json").arg("--skip-git-repo-check");
        cmd.arg(&self.prompt);

        let mut projector = StreamProjector::new();
        let line_to_events = move |line: &str| -> Vec<AgentEvent> {
            let raw: CodexStreamEvent = match serde_json::from_str(line) {
                Ok(r) => r,
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
