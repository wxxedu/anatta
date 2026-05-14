//! Path computation for claude working files / sidecar directories.
//!
//! Claude expects session JSONLs at:
//!   `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`
//! and large tool outputs / sub-agent transcripts at:
//!   `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>/` (a sibling dir)
//!
//! `encoded-cwd` is `cwd.canonical.to_string_lossy().replace('/', "-")`.
//! macOS canonicalizes `/tmp` → `/private/tmp` and claude respects that,
//! so the conversation's stored cwd MUST already be canonicalized.

use std::path::{Path, PathBuf};
#[cfg(feature = "spawn")]
use std::time::Duration;

/// Encode a canonical absolute path into claude's filesystem-key form:
/// every `/` AND every `.` is replaced with `-`. Empirically, claude
/// encodes both characters this way when computing
/// `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/`. Caller is responsible
/// for ensuring the path has been canonicalized (via `std::fs::canonicalize`)
/// at conversation-create time — re-canonicalizing here would fail if
/// the directory has since been removed.
pub fn encode_cwd(canonical_cwd: &str) -> String {
    canonical_cwd
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Working JSONL path: `<profile_dir>/projects/<encoded_cwd>/<session_uuid>.jsonl`.
pub fn working_jsonl_path(profile_dir: &Path, canonical_cwd: &str, session_uuid: &str) -> PathBuf {
    profile_dir
        .join("projects")
        .join(encode_cwd(canonical_cwd))
        .join(format!("{session_uuid}.jsonl"))
}

/// Working sidecar directory:
/// `<profile_dir>/projects/<encoded_cwd>/<session_uuid>/`.
pub fn working_sidecar_dir(profile_dir: &Path, canonical_cwd: &str, session_uuid: &str) -> PathBuf {
    profile_dir
        .join("projects")
        .join(encode_cwd(canonical_cwd))
        .join(session_uuid)
}

/// Poll `path` every 25 ms until it exists or `timeout` elapses.
///
/// Used by the interactive PTY spawn flow to know when claude has
/// actually created its session JSONL — at which point claude is past
/// startup and ready to receive a prompt over the PTY master.
///
/// Returns `Ok(())` as soon as the file exists, `Err` on timeout.
#[cfg(feature = "spawn")]
pub async fn wait_for_jsonl(path: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let interval = Duration::from_millis(25);
    loop {
        if path.exists() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("jsonl did not appear at {} within {:?}", path.display(), timeout),
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn encode_cwd_simple() {
        assert_eq!(
            encode_cwd("/Users/wangxiuxuan/Developer/anatta"),
            "-Users-wangxiuxuan-Developer-anatta",
        );
    }

    #[test]
    fn encode_cwd_root() {
        assert_eq!(encode_cwd("/"), "-");
    }

    #[test]
    fn encode_cwd_with_dots_and_dashes() {
        assert_eq!(
            encode_cwd("/private/tmp/anatta-compact-test"),
            "-private-tmp-anatta-compact-test",
        );
    }

    #[test]
    fn encode_cwd_replaces_dots_too() {
        // Empirically observed: claude encodes both '/' and '.' as '-'.
        assert_eq!(
            encode_cwd("/private/var/folders/T/.tmpfoo"),
            "-private-var-folders-T--tmpfoo",
        );
        assert_eq!(
            encode_cwd("/Users/me/repo/.claude/worktrees/wt"),
            "-Users-me-repo--claude-worktrees-wt",
        );
    }

    #[test]
    fn jsonl_path_layout() {
        let p = working_jsonl_path(
            &PathBuf::from("/profile/dir"),
            "/Users/wxx/code",
            "abcd-1234",
        );
        assert_eq!(
            p,
            PathBuf::from("/profile/dir/projects/-Users-wxx-code/abcd-1234.jsonl"),
        );
    }

    #[test]
    fn sidecar_path_layout() {
        let p = working_sidecar_dir(
            &PathBuf::from("/profile/dir"),
            "/Users/wxx/code",
            "abcd-1234",
        );
        assert_eq!(
            p,
            PathBuf::from("/profile/dir/projects/-Users-wxx-code/abcd-1234"),
        );
    }

    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_returns_immediately_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ready.jsonl");
        std::fs::write(&path, "").unwrap();
        let start = std::time::Instant::now();
        let res = super::wait_for_jsonl(&path, std::time::Duration::from_secs(5)).await;
        assert!(res.is_ok());
        assert!(start.elapsed() < std::time::Duration::from_millis(200));
    }

    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_returns_when_file_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("delayed.jsonl");
        let path_for_writer = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            std::fs::write(path_for_writer, "").unwrap();
        });
        let res = super::wait_for_jsonl(&path, std::time::Duration::from_secs(2)).await;
        assert!(res.is_ok());
    }

    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("never.jsonl");
        let res = super::wait_for_jsonl(&path, std::time::Duration::from_millis(150)).await;
        assert!(res.is_err());
    }
}
