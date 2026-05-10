//! Codex-specific profile: a `CODEX_HOME` with shared `sessions/`.

use std::path::{Path, PathBuf};

use super::{create_dir_all, profile_dir, symlink_dir, CodexProfileId, ProfileError};

/// A codex profile owns a directory tree:
///
/// ```text
/// $anatta_root/profiles/codex-{id}/
/// ├── sessions/  ──────►  $anatta_root/shared/codex-sessions/   (symlink)
/// └── (auth.json, config.toml — codex writes them on auth / config)
/// ```
///
/// Each profile has its own `auth.json` (account credentials), but
/// `sessions/` is symlinked to a shared store so a thread started on
/// one profile can be `codex exec resume`-d on another (different
/// account, same thread state).
#[derive(Debug, Clone)]
pub struct CodexProfile {
    pub id: CodexProfileId,
    pub path: PathBuf,
}

impl CodexProfile {
    /// Create the on-disk profile structure. Idempotent.
    pub fn create(id: CodexProfileId, anatta_root: &Path) -> Result<Self, ProfileError> {
        let dir = profile_dir(anatta_root, id.as_str());
        create_dir_all(&dir)?;

        let shared_sessions = anatta_root.join("shared").join("codex-sessions");
        create_dir_all(&shared_sessions)?;

        let sessions_link = dir.join("sessions");
        if !sessions_link.exists() {
            symlink_dir(&shared_sessions, &sessions_link)?;
        }

        Ok(Self { id, path: dir })
    }

    pub fn open(id: CodexProfileId, anatta_root: &Path) -> Result<Self, ProfileError> {
        let dir = profile_dir(anatta_root, id.as_str());
        if !dir.is_dir() {
            return Err(ProfileError::NotFound(dir));
        }
        Ok(Self { id, path: dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_lays_out_profile_dir_and_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let id = CodexProfileId::new();
        let p = CodexProfile::create(id.clone(), tmp.path()).unwrap();

        assert!(p.path.is_dir());
        let sessions = p.path.join("sessions");
        assert!(sessions.symlink_metadata().unwrap().file_type().is_symlink());

        let shared = tmp.path().join("shared").join("codex-sessions");
        assert!(shared.is_dir());
        let resolved = std::fs::read_link(&sessions).unwrap();
        assert!(resolved.ends_with("shared/codex-sessions"));
    }

    #[test]
    fn create_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let id = CodexProfileId::new();
        let _ = CodexProfile::create(id.clone(), tmp.path()).unwrap();
        let _ = CodexProfile::create(id, tmp.path()).unwrap();
    }

    #[test]
    fn open_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let id = CodexProfileId::new();
        let err = CodexProfile::open(id, tmp.path()).unwrap_err();
        assert!(matches!(err, ProfileError::NotFound(_)));
    }

    #[test]
    fn cross_profile_share_same_sessions_target() {
        let tmp = tempfile::tempdir().unwrap();
        let a = CodexProfile::create(CodexProfileId::new(), tmp.path()).unwrap();
        let b = CodexProfile::create(CodexProfileId::new(), tmp.path()).unwrap();

        let a_target = std::fs::read_link(a.path.join("sessions")).unwrap();
        let b_target = std::fs::read_link(b.path.join("sessions")).unwrap();
        assert_eq!(a_target, b_target);
    }
}
