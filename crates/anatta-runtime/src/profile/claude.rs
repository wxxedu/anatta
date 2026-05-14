//! Claude-specific profile: a `CLAUDE_CONFIG_DIR` with shared `projects/`.

use std::path::{Path, PathBuf};

use super::{ClaudeProfileId, ProfileError, create_dir_all, profile_dir, symlink_dir};

/// A claude profile owns a directory tree:
///
/// ```text
/// $anatta_root/profiles/claude-{id}/
/// ├── projects/  ──────►  $anatta_root/shared/claude-projects/   (symlink)
/// └── (settings.json, .claude.json, etc. — claude writes them as needed)
/// ```
///
/// Auth lives in the macOS keychain under an entry derived from the
/// directory path (`Claude Code-credentials-<sha256(path)[:8]>`), so
/// each profile authenticates against its own claude account. `projects/`
/// being symlinked to a shared store means a session started under one
/// profile can be `--resume`-d under another (different account, same
/// conversation history).
#[derive(Debug, Clone)]
pub struct ClaudeProfile {
    pub id: ClaudeProfileId,
    pub path: PathBuf,
}

impl ClaudeProfile {
    /// Create the on-disk profile structure.
    ///
    /// Idempotent: if the profile directory already exists with the
    /// expected `projects/` symlink, return without touching anything.
    /// Missing pieces are filled in.
    pub fn create(id: ClaudeProfileId, anatta_root: &Path) -> Result<Self, ProfileError> {
        let dir = profile_dir(anatta_root, id.as_str());
        create_dir_all(&dir)?;

        let shared_projects = anatta_root.join("shared").join("claude-projects");
        create_dir_all(&shared_projects)?;

        let projects_link = dir.join("projects");
        if !projects_link.exists() {
            symlink_dir(&shared_projects, &projects_link)?;
        }

        Ok(Self { id, path: dir })
    }

    /// Open an existing profile. Returns `NotFound` if the directory
    /// doesn't exist; does no further validation (callers can probe
    /// the symlink / keychain entry as needed).
    pub fn open(id: ClaudeProfileId, anatta_root: &Path) -> Result<Self, ProfileError> {
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
        let id = ClaudeProfileId::new();
        let p = ClaudeProfile::create(id.clone(), tmp.path()).unwrap();

        assert!(
            p.path.is_dir(),
            "profile dir not created: {}",
            p.path.display()
        );
        let projects = p.path.join("projects");
        assert!(projects.exists(), "projects/ link missing");
        assert!(
            projects
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "projects/ should be a symlink"
        );

        let shared = tmp.path().join("shared").join("claude-projects");
        assert!(shared.is_dir(), "shared dir not created");
        let resolved = std::fs::read_link(&projects).unwrap();
        assert!(
            resolved.ends_with("shared/claude-projects"),
            "symlink target unexpected: {}",
            resolved.display()
        );
    }

    #[test]
    fn create_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ClaudeProfileId::new();
        let _ = ClaudeProfile::create(id.clone(), tmp.path()).unwrap();
        let _ = ClaudeProfile::create(id.clone(), tmp.path()).unwrap();
        // Should not panic / error on second call.
    }

    #[test]
    fn open_existing_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ClaudeProfileId::new();
        let _ = ClaudeProfile::create(id.clone(), tmp.path()).unwrap();
        let opened = ClaudeProfile::open(id.clone(), tmp.path()).unwrap();
        assert_eq!(opened.id, id);
    }

    #[test]
    fn open_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ClaudeProfileId::new();
        let err = ClaudeProfile::open(id, tmp.path()).unwrap_err();
        assert!(matches!(err, ProfileError::NotFound(_)));
    }

    #[test]
    fn cross_profile_share_same_projects_target() {
        let tmp = tempfile::tempdir().unwrap();
        let a = ClaudeProfile::create(ClaudeProfileId::new(), tmp.path()).unwrap();
        let b = ClaudeProfile::create(ClaudeProfileId::new(), tmp.path()).unwrap();

        let a_target = std::fs::read_link(a.path.join("projects")).unwrap();
        let b_target = std::fs::read_link(b.path.join("projects")).unwrap();
        assert_eq!(
            a_target, b_target,
            "both profiles should share the same projects/ target"
        );
    }
}
