//! Per-conversation lock with RAII guard + libc-backed PID liveness.
//!
//! Lock state is in the `conversations.lock_holder_pid` column. Acquire
//! is a single `BEGIN IMMEDIATE` transaction (see
//! `anatta_store::conversation::try_acquire_with_check`). Release SQL is
//! keyed on `(name, holder_pid)` so a delayed Drop after a force-unlock
//! + reacquire by another process becomes a no-op.

use anatta_store::conversation::AcquireOutcome;
use anatta_store::Store;

use super::ChatError;

/// RAII guard for a conversation lock. Construct via
/// [`ConversationGuard::acquire`]; release explicitly via
/// [`release_now`](Self::release_now) on the happy path or rely on Drop
/// for `?` / panic propagation paths.
pub(crate) struct ConversationGuard<'a> {
    store: &'a Store,
    name: String,
    holder_pid: i64,
    released: bool,
}

impl<'a> ConversationGuard<'a> {
    pub(crate) async fn acquire(store: &'a Store, name: &str) -> Result<Self, ChatError> {
        let my_pid = std::process::id() as i64;
        match store
            .try_acquire_with_check(name, my_pid, pid_alive)
            .await?
        {
            AcquireOutcome::Acquired => Ok(Self {
                store,
                name: name.to_owned(),
                holder_pid: my_pid,
                released: false,
            }),
            AcquireOutcome::Held { pid } => Err(ChatError::Locked {
                name: name.to_owned(),
                pid,
            }),
        }
    }

    /// Explicit async release. Call this on the happy path before the
    /// guard goes out of scope. Drop is a best-effort fallback for the
    /// panic / `?` paths.
    pub(crate) async fn release_now(mut self) -> Result<(), ChatError> {
        self.store
            .release_lock_if_held(&self.name, self.holder_pid)
            .await?;
        self.released = true;
        Ok(())
    }
}

impl Drop for ConversationGuard<'_> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Best-effort detached release. Drop can't be async and the
        // current runtime may be tearing down. Failure → warn and let
        // the next acquirer's PID-liveness check reclaim.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let store: Store = (*self.store).clone();
            let name = std::mem::take(&mut self.name);
            let pid = self.holder_pid;
            handle.spawn(async move {
                if let Err(e) = store.release_lock_if_held(&name, pid).await {
                    eprintln!(
                        "[anatta] warn: failed to release lock for '{name}': {e}"
                    );
                }
            });
        }
    }
}

/// PID liveness check. `true` means "this process exists / may exist".
///
/// Unix: `kill(pid, 0)` is the canonical existence check. `ESRCH` (no
/// such process) → dead; `EPERM` (process exists but we can't signal)
/// → alive. Windows: conservative `true`; user must `chat unlock`.
pub(crate) fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if pid > libc::pid_t::MAX as i64 {
            return false;
        }
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        // EPERM: process exists, we just can't signal it. ESRCH: no such pid.
        errno == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_rejects_zero_and_negative() {
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn pid_alive_recognizes_self_process() {
        let pid = std::process::id() as i64;
        assert!(pid_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_dead_pid_returns_false() {
        // PID 1_000_000_000+ is almost certainly never assigned on a normal box.
        // On Linux, PID_MAX is typically 4M; on macOS, 99999. Either way this
        // pid is a confidently-absent slot.
        assert!(!pid_alive(2_000_000_000));
    }
}
