//! Auth flow primitives shared by `profile create`.
//!
//! Two paths:
//!
//!   * **login** — spawn the backend's auth subcommand
//!     (`claude auth login` / `codex login`) with the profile's
//!     `CLAUDE_CONFIG_DIR` / `CODEX_HOME` set, inheriting our TTY so
//!     the user can complete the browser handshake. We just wait for
//!     the child to exit cleanly.
//!
//!   * **api_key** — prompt the user for the key, store it in the OS
//!     keyring under (service="anatta", account=profile_id). At
//!     spawn-time later, the orchestrator reads it back and sets
//!     `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` on the launched process.

use std::path::Path;
use std::process::Stdio;

use anatta_store::profile::BackendKind;

const KEYRING_SERVICE: &str = "anatta";

/// Run the backend's interactive login subcommand against the given
/// profile dir. Inherits our stdin/stdout/stderr so the user can
/// complete the browser flow.
pub async fn run_login(
    backend: BackendKind,
    profile_dir: &Path,
    binary_path: &Path,
) -> Result<(), AuthError> {
    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    match backend {
        BackendKind::Claude => {
            cmd.env("CLAUDE_CONFIG_DIR", profile_dir);
            cmd.arg("auth").arg("login");
        }
        BackendKind::Codex => {
            cmd.env("CODEX_HOME", profile_dir);
            cmd.arg("login");
        }
    }
    let status = cmd.status().await.map_err(AuthError::Spawn)?;
    if !status.success() {
        return Err(AuthError::LoginFailed {
            backend,
            code: status.code(),
        });
    }
    Ok(())
}

/// Store an API key in the OS keyring under (service, account=profile_id).
/// Overwrites any existing entry for the same id.
pub fn store_api_key(profile_id: &str, key: &str) -> Result<(), AuthError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id).map_err(AuthError::Keyring)?;
    entry.set_password(key).map_err(AuthError::Keyring)?;
    Ok(())
}

/// Look up an API key by profile id. `Ok(None)` if not present.
pub fn read_api_key(profile_id: &str) -> Result<Option<String>, AuthError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id).map_err(AuthError::Keyring)?;
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AuthError::Keyring(e)),
    }
}

/// Remove an API key entry. Idempotent — silently succeeds if absent.
pub fn delete_api_key(profile_id: &str) -> Result<(), AuthError> {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, profile_id) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(AuthError::Keyring(e)),
    }
}

/// Find a binary by name on $PATH. Returns `Ok(None)` if not found.
pub fn locate_binary(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("could not spawn backend binary: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("{backend:?} auth login exited with code {code:?}")]
    LoginFailed {
        backend: BackendKind,
        code: Option<i32>,
    },
    #[error("OS keyring: {0}")]
    Keyring(#[source] keyring::Error),
}
