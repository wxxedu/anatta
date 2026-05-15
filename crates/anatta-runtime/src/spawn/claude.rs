//! Claude `--print --output-format stream-json` launch.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anatta_core::{AgentEvent, ProjectionContext, Projector};
use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;

use super::pipeline::{PipelineHandles, spawn_with_pipeline};
use super::{AgentSession, ClaudeSessionId, Launchable, SpawnError};
use crate::claude::StreamProjector;
use crate::claude::stream::ClaudeStreamEvent;
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
    /// Initial permission level. Mapped to `--permission-mode <value>`
    /// at spawn. The per-turn shape re-spawns claude on every turn, so
    /// updating this between turns takes effect on the next turn.
    pub permission_level: anatta_core::PermissionLevel,
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

        if self.permission_level == anatta_core::PermissionLevel::BypassAll {
            ensure_skip_dangerous_mode_permission_prompt(&self.profile.path).await?;
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
            // anatta orchestrates the conversation lifecycle; claude's
            // interactive permission prompts are never visible in
            // `--print` mode and would stall the turn. We pass the
            // current PermissionLevel as the explicit mode so the
            // backend behaves consistently with the interactive shape.
            .arg("--permission-mode")
            .arg(self.permission_level.claude_arg());
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

/// Pre-seed Claude's current bypass-permissions acknowledgment marker.
///
/// Claude 2.1.x shows a blocking TUI safety dialog before enabling
/// `bypassPermissions` unless `<CLAUDE_CONFIG_DIR>/settings.json` contains
/// this key. Anatta cannot answer that dialog through the discarded PTY, so
/// bypass-mode launches must make the profile explicit up front.
pub(crate) async fn ensure_skip_dangerous_mode_permission_prompt(
    profile_dir: &Path,
) -> Result<(), SpawnError> {
    let settings_json = profile_dir.join("settings.json");
    let mut settings: serde_json::Value = match tokio::fs::read_to_string(&settings_json).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(e) if e.kind() == ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(SpawnError::Io(e)),
    };
    let Some(obj) = settings.as_object_mut() else {
        return Ok(());
    };

    if obj
        .get("skipDangerousModePermissionPrompt")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Ok(());
    }

    obj.insert(
        "skipDangerousModePermissionPrompt".to_string(),
        serde_json::Value::Bool(true),
    );
    let pretty = serde_json::to_string_pretty(&settings).map_err(|e| {
        SpawnError::Io(std::io::Error::other(format!(
            "serialize claude settings: {e}"
        )))
    })?;
    tokio::fs::write(settings_json, pretty)
        .await
        .map_err(SpawnError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ensure_skip_dangerous_mode_permission_prompt_creates_settings() {
        let tmp = tempfile::tempdir().unwrap();

        ensure_skip_dangerous_mode_permission_prompt(tmp.path())
            .await
            .unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings
                .get("skipDangerousModePermissionPrompt")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn ensure_skip_dangerous_mode_permission_prompt_preserves_settings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();

        ensure_skip_dangerous_mode_permission_prompt(tmp.path())
            .await
            .unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["permissions"]["defaultMode"], "auto");
        assert_eq!(
            settings
                .get("skipDangerousModePermissionPrompt")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }
}
