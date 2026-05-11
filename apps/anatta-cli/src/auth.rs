//! Auth flow primitives shared by `profile create`.
//!
//! Two paths:
//!
//!   * **login** — spawn the backend's auth subcommand
//!     (`claude auth login` / `codex login`) with the profile's
//!     `CLAUDE_CONFIG_DIR` / `CODEX_HOME` set, inheriting our TTY so
//!     the user can complete the browser handshake. The backend writes
//!     its own credentials into the profile dir.
//!
//!   * **api_key** — prompt the user for the key, store it in
//!     `<profile_dir>/anatta-credentials.json` (mode 0600). At
//!     spawn-time later, [`read_api_key`] reads that file once and
//!     supplies the token to the launched process via
//!     `ANTHROPIC_AUTH_TOKEN` / `OPENAI_API_KEY`.
//!
//! Credentials live in the profile dir so a profile is a
//! self-contained unit on disk (`rm -rf <profile_dir>` cleanly removes
//! everything). Storage is plain JSON 600 — same model as codex's
//! own `auth.json` for OAuth tokens. The OS releases nothing extra on
//! top of POSIX mode; relying on FileVault / LUKS for disk-image
//! attacks is the user's job.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anatta_store::profile::BackendKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const CREDENTIALS_FILENAME: &str = "anatta-credentials.json";
const CURRENT_VERSION: u32 = 1;

/// On-disk credentials document. Layout is intentionally tiny and
/// versioned so future fields (refresh tokens, fingerprint, etc.) can
/// be added without breaking older readers.
#[derive(Debug, Serialize, Deserialize)]
struct Credentials {
    version: u32,
    api_key: String,
    stored_at: DateTime<Utc>,
}

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

/// Compute the credentials file path under `<anatta_home>/profiles/<id>/`.
pub fn credentials_path(anatta_home: &Path, profile_id: &str) -> PathBuf {
    anatta_home
        .join("profiles")
        .join(profile_id)
        .join(CREDENTIALS_FILENAME)
}

/// Store an API key in `<profile_dir>/anatta-credentials.json` with
/// mode 0600. Atomic (write-to-temp + rename) to avoid corruption on
/// crash mid-write. Overwrites any existing entry.
///
/// The profile directory is expected to already exist (created by the
/// runtime's `ClaudeProfile::create` / `CodexProfile::create`).
pub fn store_api_key(anatta_home: &Path, profile_id: &str, key: &str) -> Result<(), AuthError> {
    let path = credentials_path(anatta_home, profile_id);
    let dir = path.parent().ok_or_else(|| {
        AuthError::Io(std::io::Error::other(
            "credentials path has no parent directory",
        ))
    })?;
    if !dir.is_dir() {
        return Err(AuthError::ProfileDirMissing(dir.to_path_buf()));
    }
    let creds = Credentials {
        version: CURRENT_VERSION,
        api_key: key.to_owned(),
        stored_at: Utc::now(),
    };
    let body = serde_json::to_vec_pretty(&creds).map_err(AuthError::Serde)?;

    // Atomic write: temp file in same dir + rename.
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        set_mode_0600(&mut opts);
        let mut f: File = opts.open(&tmp).map_err(AuthError::Io)?;
        f.write_all(&body).map_err(AuthError::Io)?;
        f.flush().map_err(AuthError::Io)?;
        // Best-effort sync; not all FS support it (e.g. tmpfs).
        let _ = f.sync_all();
    }
    // Belt-and-suspenders: enforce 0600 explicitly in case OpenOptions
    // ignored our mode (e.g. on Windows).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).map_err(AuthError::Io)?;
    Ok(())
}

/// Read the API key for `profile_id`. `Ok(None)` means there is no
/// credentials file. Refuses to read if the file's POSIX mode is too
/// permissive (group/other have any bits set) — analogous to ssh's
/// rejection of world-readable private keys.
pub fn read_api_key(anatta_home: &Path, profile_id: &str) -> Result<Option<String>, AuthError> {
    let path = credentials_path(anatta_home, profile_id);
    if !path.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(&path).map_err(AuthError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(AuthError::CredentialsTooOpen {
                path: path.clone(),
                mode,
            });
        }
    }
    let _ = meta; // suppress unused on non-unix

    let mut file = File::open(&path).map_err(AuthError::Io)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).map_err(AuthError::Io)?;
    let creds: Credentials = serde_json::from_str(&buf).map_err(AuthError::Serde)?;
    if creds.version != CURRENT_VERSION {
        return Err(AuthError::UnsupportedCredentialsVersion {
            path,
            got: creds.version,
            expected: CURRENT_VERSION,
        });
    }
    Ok(Some(creds.api_key))
}

/// Remove the credentials file. Idempotent — silently succeeds if absent.
pub fn delete_api_key(anatta_home: &Path, profile_id: &str) -> Result<(), AuthError> {
    let path = credentials_path(anatta_home, profile_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AuthError::Io(e)),
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

#[cfg(unix)]
fn set_mode_0600(opts: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
}

#[cfg(not(unix))]
fn set_mode_0600(_opts: &mut OpenOptions) {
    // On Windows we rely on the default ACL (current user only) and
    // the user's own filesystem permissions. There is no exact 0o600
    // analog; group/world bits don't exist.
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
    #[error("profile directory missing: {0}")]
    ProfileDirMissing(PathBuf),
    #[error(
        "credentials file {path} has permissions {mode:o}, expected 600 — refusing to read; \
         run `chmod 600 {path}` to fix"
    )]
    CredentialsTooOpen { path: PathBuf, mode: u32 },
    #[error("credentials file {path} has version {got}, this build expects {expected}")]
    UnsupportedCredentialsVersion {
        path: PathBuf,
        got: u32,
        expected: u32,
    },
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_profile_dir(home: &Path, id: &str) -> PathBuf {
        let dir = home.join("profiles").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_store_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-AbCd");
        store_api_key(tmp.path(), "claude-AbCd", "sk-test-123").unwrap();
        let got = read_api_key(tmp.path(), "claude-AbCd").unwrap();
        assert_eq!(got.as_deref(), Some("sk-test-123"));
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-Missing");
        let got = read_api_key(tmp.path(), "claude-Missing").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-X");
        delete_api_key(tmp.path(), "claude-X").unwrap(); // no file yet
        store_api_key(tmp.path(), "claude-X", "key").unwrap();
        delete_api_key(tmp.path(), "claude-X").unwrap();
        delete_api_key(tmp.path(), "claude-X").unwrap(); // again — still ok
        assert!(read_api_key(tmp.path(), "claude-X").unwrap().is_none());
    }

    #[test]
    fn overwrite_existing_key() {
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-Y");
        store_api_key(tmp.path(), "claude-Y", "k1").unwrap();
        store_api_key(tmp.path(), "claude-Y", "k2").unwrap();
        assert_eq!(
            read_api_key(tmp.path(), "claude-Y").unwrap().as_deref(),
            Some("k2"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-Z");
        store_api_key(tmp.path(), "claude-Z", "k").unwrap();
        let meta = std::fs::metadata(credentials_path(tmp.path(), "claude-Z")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got mode {:o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn read_refuses_too_open_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-Loose");
        store_api_key(tmp.path(), "claude-Loose", "k").unwrap();
        // Chmod to 644 deliberately.
        let path = credentials_path(tmp.path(), "claude-Loose");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_api_key(tmp.path(), "claude-Loose").unwrap_err();
        assert!(matches!(err, AuthError::CredentialsTooOpen { .. }));
    }

    #[test]
    fn store_fails_when_profile_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No profile dir created.
        let err = store_api_key(tmp.path(), "claude-Nope", "k").unwrap_err();
        assert!(matches!(err, AuthError::ProfileDirMissing(_)));
    }

    #[test]
    fn read_rejects_unsupported_version() {
        let tmp = tempfile::tempdir().unwrap();
        fake_profile_dir(tmp.path(), "claude-V99");
        let path = credentials_path(tmp.path(), "claude-V99");
        std::fs::write(
            &path,
            r#"{"version":99,"api_key":"x","stored_at":"2026-05-11T00:00:00Z"}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = read_api_key(tmp.path(), "claude-V99").unwrap_err();
        assert!(matches!(err, AuthError::UnsupportedCredentialsVersion { .. }));
    }
}
