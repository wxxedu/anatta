//! Per-Intent profile management for backend CLIs.
//!
//! A profile is an isolated `CLAUDE_CONFIG_DIR` / `CODEX_HOME` with its
//! own credential keychain entry / `auth.json`, but with `projects/` /
//! `sessions/` symlinked to a shared store so resume across profiles
//! works without copying session files.
//!
//! IDs are typed per-backend ([`ClaudeProfileId`], [`CodexProfileId`]),
//! prefix-qualified, and filesystem-safe. They are generated once and
//! stable; user-facing names live in daemon-core's database, mapped to
//! these ids. Cross-backend code paths that need to handle either type
//! use [`AnyProfileId`].

mod claude;
mod codex;
pub mod family;
pub mod policy;
pub mod providers;

pub use claude::ClaudeProfile;
pub use codex::CodexProfile;
pub use family::{default_family, family_of, BackendKind, Family};
pub use policy::{min_policy_for, CompactSummary, SegmentRenderPolicy};
pub use providers::{Overrides, ProviderEnv, ProviderSpec, Tier};

use std::fmt;
use std::path::PathBuf;

/// Failures from profile creation, opening, or id parsing.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("profile id has invalid format: {0:?}")]
    InvalidIdFormat(String),
    #[error("profile not found: {0}")]
    NotFound(PathBuf),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

const ID_NANOID_LEN: usize = 8;

const CLAUDE_PREFIX: &str = "claude-";
const CODEX_PREFIX: &str = "codex-";

// ────────────────────────────────────────────────────────────────────────────
// ClaudeProfileId
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeProfileId(String);

impl ClaudeProfileId {
    /// Generate a fresh id of the form `claude-{8 url-safe chars}`.
    pub fn new() -> Self {
        Self(format!("{CLAUDE_PREFIX}{}", nanoid::nanoid!(ID_NANOID_LEN)))
    }

    /// Parse a string into an id, validating the prefix.
    pub fn from_string(s: String) -> Result<Self, ProfileError> {
        if !s.starts_with(CLAUDE_PREFIX) || s.len() <= CLAUDE_PREFIX.len() {
            return Err(ProfileError::InvalidIdFormat(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ClaudeProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ClaudeProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// CodexProfileId
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodexProfileId(String);

impl CodexProfileId {
    pub fn new() -> Self {
        Self(format!("{CODEX_PREFIX}{}", nanoid::nanoid!(ID_NANOID_LEN)))
    }

    pub fn from_string(s: String) -> Result<Self, ProfileError> {
        if !s.starts_with(CODEX_PREFIX) || s.len() <= CODEX_PREFIX.len() {
            return Err(ProfileError::InvalidIdFormat(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for CodexProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CodexProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AnyProfileId
// ────────────────────────────────────────────────────────────────────────────

/// Cross-backend profile id. Use this where heterogeneous profiles
/// share one storage column, e.g. daemon-core's profile registry / DB.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnyProfileId {
    Claude(ClaudeProfileId),
    Codex(CodexProfileId),
}

impl AnyProfileId {
    /// Discriminate on prefix and parse into the right variant.
    pub fn parse(s: &str) -> Result<Self, ProfileError> {
        if s.starts_with(CLAUDE_PREFIX) {
            Ok(Self::Claude(ClaudeProfileId::from_string(s.to_owned())?))
        } else if s.starts_with(CODEX_PREFIX) {
            Ok(Self::Codex(CodexProfileId::from_string(s.to_owned())?))
        } else {
            Err(ProfileError::InvalidIdFormat(s.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude(id) => id.as_str(),
            Self::Codex(id) => id.as_str(),
        }
    }
}

impl fmt::Display for AnyProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Path helpers shared by both backends
// ────────────────────────────────────────────────────────────────────────────

pub(super) fn profile_dir(anatta_root: &std::path::Path, id: &str) -> PathBuf {
    anatta_root.join("profiles").join(id)
}

#[cfg(unix)]
pub(super) fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> Result<(), ProfileError> {
    std::os::unix::fs::symlink(target, link).map_err(|e| ProfileError::Io {
        path: link.to_owned(),
        source: e,
    })
}

#[cfg(windows)]
pub(super) fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> Result<(), ProfileError> {
    std::os::windows::fs::symlink_dir(target, link).map_err(|e| ProfileError::Io {
        path: link.to_owned(),
        source: e,
    })
}

pub(super) fn create_dir_all(path: &std::path::Path) -> Result<(), ProfileError> {
    std::fs::create_dir_all(path).map_err(|e| ProfileError::Io {
        path: path.to_owned(),
        source: e,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_id_format_invariant() {
        let id = ClaudeProfileId::new();
        assert!(id.as_str().starts_with("claude-"));
        assert_eq!(id.as_str().len(), CLAUDE_PREFIX.len() + ID_NANOID_LEN);
    }

    #[test]
    fn codex_id_format_invariant() {
        let id = CodexProfileId::new();
        assert!(id.as_str().starts_with("codex-"));
        assert_eq!(id.as_str().len(), CODEX_PREFIX.len() + ID_NANOID_LEN);
    }

    #[test]
    fn ids_are_unique_in_practice() {
        let a = ClaudeProfileId::new();
        let b = ClaudeProfileId::new();
        assert_ne!(a, b, "two fresh ids should not collide");
    }

    #[test]
    fn claude_id_round_trip_via_from_string() {
        let id = ClaudeProfileId::new();
        let recovered = ClaudeProfileId::from_string(id.as_str().to_owned()).unwrap();
        assert_eq!(id, recovered);
    }

    #[test]
    fn claude_id_rejects_wrong_prefix() {
        let err = ClaudeProfileId::from_string("codex-AbCd1234".into()).unwrap_err();
        assert!(matches!(err, ProfileError::InvalidIdFormat(_)));
    }

    #[test]
    fn claude_id_rejects_empty_after_prefix() {
        let err = ClaudeProfileId::from_string("claude-".into()).unwrap_err();
        assert!(matches!(err, ProfileError::InvalidIdFormat(_)));
    }

    #[test]
    fn any_id_parses_claude() {
        let any = AnyProfileId::parse("claude-AbCd1234").unwrap();
        assert!(matches!(any, AnyProfileId::Claude(_)));
        assert_eq!(any.as_str(), "claude-AbCd1234");
    }

    #[test]
    fn any_id_parses_codex() {
        let any = AnyProfileId::parse("codex-XyZw9876").unwrap();
        assert!(matches!(any, AnyProfileId::Codex(_)));
    }

    #[test]
    fn any_id_rejects_unknown_prefix() {
        assert!(AnyProfileId::parse("gpt-AbCd1234").is_err());
        assert!(AnyProfileId::parse("AbCd1234").is_err());
        assert!(AnyProfileId::parse("").is_err());
    }

    #[test]
    fn display_outputs_inner_string() {
        let id = ClaudeProfileId::from_string("claude-Test1234".into()).unwrap();
        assert_eq!(format!("{id}"), "claude-Test1234");
    }
}
