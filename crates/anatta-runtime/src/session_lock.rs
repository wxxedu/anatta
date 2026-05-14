//! Cross-process exclusive lock on a backend session.
//!
//! anatta protects an underlying claude/codex on-disk session file
//! against concurrent writers by holding a per-conversation
//! [`SessionLock`]. The lock is an `flock(LOCK_EX | LOCK_NB)` taken on
//! a sidecar file under `<anatta_home>/runtime-locks/`. The OS
//! releases the lock the moment the holding process exits — there is
//! no "stale lock" state to recover from, no PID tracking, no manual
//! `unlock` command.
//!
//! Key choice: caller-supplied opaque string (we use the
//! conversation `name`). The lockfile is named after a sanitized
//! form, so `ls runtime-locks/` is still mostly human-readable.
//!
//! Drop on the [`SessionLock`] closes the underlying file descriptor
//! which the OS interprets as "release this flock". No async-safe
//! teardown needed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// An exclusive lock for one logical session. While this value lives,
/// no other process can `try_acquire` the same `key` on the same
/// `anatta_home`.
#[derive(Debug)]
pub struct SessionLock {
    _file: File,
    key: String,
    path: PathBuf,
}

impl SessionLock {
    /// Try to take an exclusive lock for `key` under `anatta_home`.
    ///
    /// Returns [`LockError::Held`] immediately if another process owns
    /// the lock. Returns [`LockError::Io`] on filesystem failures
    /// (missing perms, disk full, etc.).
    ///
    /// The held lock is released when this value drops — including
    /// implicit drop on panic / `?` propagation. No explicit
    /// release_now is needed; the OS releases when the process exits
    /// even via SIGKILL, so there is no stale-lock state.
    pub fn try_acquire(anatta_home: &Path, key: &str) -> Result<Self, LockError> {
        let dir = anatta_home.join("runtime-locks");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.lock", sanitize_for_filename(key)));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => {
                // Best-effort: write holder metadata so manual
                // inspection (`cat <lockfile>`) shows pid/time/key.
                // Failure here is non-fatal — we already hold the lock.
                let _ = write_owner_info(&file, key);
                Ok(Self {
                    _file: file,
                    key: key.to_owned(),
                    path,
                })
            }
            Ok(false) => Err(LockError::Held {
                key: key.to_owned(),
            }),
            Err(e) => Err(LockError::Io(e)),
        }
    }

    /// Non-blocking probe: is the lock for `key` held by anyone?
    ///
    /// Opens the lockfile, tries a shared lock, drops everything. A
    /// success means no exclusive lock is currently held; a failure
    /// with WouldBlock means someone holds exclusive. Useful for
    /// `chat ls` style status display.
    pub fn is_held(anatta_home: &Path, key: &str) -> bool {
        let path = anatta_home
            .join("runtime-locks")
            .join(format!("{}.lock", sanitize_for_filename(key)));
        if !path.exists() {
            return false;
        }
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        match FileExt::try_lock_shared(&file) {
            Ok(true) => false, // got shared → no exclusive holder
            Ok(false) => true, // someone holds exclusive
            Err(_) => false,   // unknown → don't claim "held"
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn lockfile_path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("session '{key}' is in use by another process")]
    Held { key: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ──────────────────────────────────────────────────────────────────────
// helpers
// ──────────────────────────────────────────────────────────────────────

/// Encode `s` into a filename-safe string. Non-alphanumeric (other
/// than `-` and `_`) bytes become `_<hex>_`. Round-tripping isn't
/// required — we only need a deterministic, collision-free mapping
/// for `ls runtime-locks/` to remain useful.
fn sanitize_for_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            for b in c.to_string().bytes() {
                use std::fmt::Write as _;
                let _ = write!(out, "_{:02x}", b);
            }
        }
    }
    out
}

fn write_owner_info(file: &File, key: &str) -> std::io::Result<()> {
    let pid = std::process::id();
    let now = chrono::Utc::now().to_rfc3339();
    let mut f = file.try_clone()?;
    f.set_len(0)?;
    writeln!(f, "pid={pid}")?;
    writeln!(f, "acquired_at={now}")?;
    writeln!(f, "key={key}")?;
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_drop_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = SessionLock::try_acquire(tmp.path(), "foo").unwrap();
        assert_eq!(lock.key(), "foo");
        assert!(SessionLock::is_held(tmp.path(), "foo"));
        drop(lock);
        assert!(!SessionLock::is_held(tmp.path(), "foo"));
    }

    #[test]
    fn second_acquire_while_held_returns_held() {
        let tmp = tempfile::tempdir().unwrap();
        let _first = SessionLock::try_acquire(tmp.path(), "foo").unwrap();
        let err = SessionLock::try_acquire(tmp.path(), "foo").unwrap_err();
        assert!(matches!(err, LockError::Held { .. }));
    }

    #[test]
    fn different_keys_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let _a = SessionLock::try_acquire(tmp.path(), "alpha").unwrap();
        let _b = SessionLock::try_acquire(tmp.path(), "beta").unwrap();
    }

    #[test]
    fn is_held_returns_false_when_no_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!SessionLock::is_held(tmp.path(), "never-locked"));
    }

    #[test]
    fn sanitize_keeps_simple_names() {
        assert_eq!(sanitize_for_filename("my-project_42"), "my-project_42");
    }

    #[test]
    fn sanitize_escapes_specials() {
        assert_eq!(sanitize_for_filename("a b"), "a_20b");
        assert_eq!(sanitize_for_filename("foo/bar"), "foo_2fbar");
    }

    #[test]
    fn lockfile_carries_owner_info() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = SessionLock::try_acquire(tmp.path(), "metadata-test").unwrap();
        let contents = std::fs::read_to_string(lock.lockfile_path()).unwrap();
        assert!(contents.contains("pid="));
        assert!(contents.contains("acquired_at="));
        assert!(contents.contains("key=metadata-test"));
    }
}
