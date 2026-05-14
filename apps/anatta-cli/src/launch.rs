//! `build_launch` — resolve a stored [`ProfileRecord`] into a fully-baked
//! [`BackendLaunch`] the runtime can consume.
//!
//! This is the one place we touch:
//!   * profile dir (`<anatta_home>/profiles/<id>/` for `CLAUDE_CONFIG_DIR`
//!     / `CODEX_HOME`)
//!   * credentials file (`anatta-credentials.json`) for api-key profiles
//!   * provider routing (model + base_url + vendor env, e.g. deepseek
//!     piggy-backing on the claude binary)
//!   * binary path on `$PATH`
//!
//! Callers pass an optional `resume` (claude session UUID / codex
//! thread UUID). The actual user prompt is supplied per turn via
//! [`Session::send_turn`](anatta_runtime::spawn::Session::send_turn);
//! the launch carries an empty prompt placeholder.

use std::path::PathBuf;

use anatta_runtime::profile::{
    ClaudeProfile, ClaudeProfileId, CodexProfile, CodexProfileId, Overrides, ProfileError,
    ProviderEnv, providers,
};
use anatta_runtime::spawn::{
    BackendLaunch, ClaudeInteractiveLaunch, ClaudeLaunch, ClaudeSessionId, CodexLaunch,
    CodexThreadId,
};
use anatta_store::profile::{AuthMethod, BackendKind, ProfileRecord};

use crate::auth;
use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("backend binary `{0}` not found on PATH")]
    BinaryNotFound(&'static str),
    #[error("api key for profile {0} is missing — run `anatta profile login` first")]
    ApiKeyMissing(String),
    #[error(
        "provider `{0}` recorded on this profile is not in the runtime registry; \
         the binary may be older than the profile"
    )]
    UnknownProvider(String),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

/// Resolve a stored profile into a [`BackendLaunch`]. Reads the
/// credentials file once (api-key profiles) and assembles the
/// per-provider env map (model overrides, base url, vendor extras).
pub fn build_launch(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<BackendLaunch, LaunchError> {
    match record.backend {
        BackendKind::Claude => {
            build_claude_interactive(record, cwd, resume, cfg).map(BackendLaunch::ClaudeInteractive)
        }
        BackendKind::Codex => build_codex(record, cwd, resume, cfg).map(BackendLaunch::Codex),
    }
}

fn read_api_key_for(record: &ProfileRecord, cfg: &Config) -> Result<Option<String>, LaunchError> {
    match record.auth_method {
        AuthMethod::Login => Ok(None),
        AuthMethod::ApiKey => Ok(Some(
            auth::read_api_key(&cfg.anatta_home, &record.id)?
                .ok_or_else(|| LaunchError::ApiKeyMissing(record.id.clone()))?,
        )),
    }
}

#[allow(dead_code)]
fn build_claude(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<ClaudeLaunch, LaunchError> {
    let id = ClaudeProfileId::from_string(record.id.clone())?;
    let profile = ClaudeProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("claude").ok_or(LaunchError::BinaryNotFound("claude"))?;

    let api_key = read_api_key_for(record, cfg)?;
    let provider = match (record.auth_method, api_key) {
        (AuthMethod::Login, _) => None,
        (AuthMethod::ApiKey, None) => return Err(LaunchError::ApiKeyMissing(record.id.clone())),
        (AuthMethod::ApiKey, Some(token)) => {
            let spec = providers::lookup(&record.provider)
                .ok_or_else(|| LaunchError::UnknownProvider(record.provider.clone()))?;
            let overrides = Overrides {
                base_url: record.base_url_override.clone(),
                model: record.model_override.clone(),
                small_fast_model: record.small_fast_model_override.clone(),
                default_opus_model: record.default_opus_model_override.clone(),
                default_sonnet_model: record.default_sonnet_model_override.clone(),
                default_haiku_model: record.default_haiku_model_override.clone(),
                subagent_model: record.subagent_model_override.clone(),
            };
            Some(ProviderEnv::build(spec, &overrides, token))
        }
    };

    Ok(ClaudeLaunch {
        profile,
        cwd,
        prompt: String::new(),
        resume: resume.map(ClaudeSessionId::new),
        binary_path,
        provider,
    })
}

fn build_claude_interactive(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<ClaudeInteractiveLaunch, LaunchError> {
    let id = ClaudeProfileId::from_string(record.id.clone())?;
    let profile = ClaudeProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("claude").ok_or(LaunchError::BinaryNotFound("claude"))?;

    let api_key = read_api_key_for(record, cfg)?;
    let provider = match (record.auth_method, api_key) {
        (AuthMethod::Login, _) => None,
        (AuthMethod::ApiKey, None) => return Err(LaunchError::ApiKeyMissing(record.id.clone())),
        (AuthMethod::ApiKey, Some(token)) => {
            let spec = providers::lookup(&record.provider)
                .ok_or_else(|| LaunchError::UnknownProvider(record.provider.clone()))?;
            let overrides = Overrides {
                base_url: record.base_url_override.clone(),
                model: record.model_override.clone(),
                small_fast_model: record.small_fast_model_override.clone(),
                default_opus_model: record.default_opus_model_override.clone(),
                default_sonnet_model: record.default_sonnet_model_override.clone(),
                default_haiku_model: record.default_haiku_model_override.clone(),
                subagent_model: record.subagent_model_override.clone(),
            };
            Some(ProviderEnv::build(spec, &overrides, token))
        }
    };

    // `--bare` is incompatible with OAuth/keychain auth (it explicitly
    // disables keychain reads). Use it only for ApiKey profiles, where
    // it gives a clean predictable environment (no hooks, no LSP, no
    // plugin sync, no CLAUDE.md auto-discovery).
    let bare = matches!(record.auth_method, AuthMethod::ApiKey);

    Ok(ClaudeInteractiveLaunch {
        profile,
        cwd,
        resume: resume.map(ClaudeSessionId::new),
        binary_path,
        provider,
        model: record.model_override.clone(),
        bare,
    })
}

fn build_codex(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<CodexLaunch, LaunchError> {
    let id = CodexProfileId::from_string(record.id.clone())?;
    let profile = CodexProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("codex").ok_or(LaunchError::BinaryNotFound("codex"))?;

    let api_key = match record.auth_method {
        AuthMethod::Login => None,
        AuthMethod::ApiKey => match read_api_key_for(record, cfg)? {
            Some(k) => Some(k),
            None => return Err(LaunchError::ApiKeyMissing(record.id.clone())),
        },
    };

    Ok(CodexLaunch {
        profile,
        cwd,
        prompt: String::new(),
        resume: resume.map(CodexThreadId::new),
        binary_path,
        api_key,
    })
}
