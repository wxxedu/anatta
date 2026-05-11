//! Per-conversation lock with RAII guard + sysinfo-backed PID liveness.
//!
//! Lock state lives in `conversations.{lock_holder_pid,
//! lock_holder_started_at}`. Acquire is a single `BEGIN IMMEDIATE`
//! transaction (see `anatta_store::conversation::try_acquire_with_check`).
//! Liveness is "same process at this PID with this start time" — a
//! reused PID has a different start time and is correctly treated as
//! stale, even across reboots.
//!
//! Release SQL is keyed on (name, holder_pid) so a delayed Drop after a
//! force-unlock + reacquire by another process becomes a no-op.

use anatta_store::conversation::AcquireOutcome;
use anatta_store::{Store, StoreError};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Why a `ConversationGuard::try_acquire` call failed.
/// Each caller maps this to its own error type (`ChatError`, `SendError`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum LockError {
    /// Lock is held by another live process.
    #[error("conversation in use by pid {pid}")]
    Held { pid: i64 },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// RAII guard for a conversation lock.
pub(crate) struct ConversationGuard<'a> {
    store: &'a Store,
    name: String,
    holder_pid: i64,
    released: bool,
}

impl<'a> ConversationGuard<'a> {
    pub(crate) async fn try_acquire(store: &'a Store, name: &str) -> Result<Self, LockError> {
        let my_pid = std::process::id() as i64;
        let my_started_at = current_process_start_time().unwrap_or(0);

        match store
            .try_acquire_with_check(name, my_pid, my_started_at, is_same_alive)
            .await?
        {
            AcquireOutcome::Acquired => Ok(Self {
                store,
                name: name.to_owned(),
                holder_pid: my_pid,
                released: false,
            }),
            AcquireOutcome::Held { pid } => Err(LockError::Held { pid }),
        }
    }

    /// Explicit async release. Call this on the happy path before the
    /// guard goes out of scope. Drop is a best-effort fallback for the
    /// panic / `?` paths.
    pub(crate) async fn release_now(mut self) -> Result<(), StoreError> {
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

/// Snapshot the current process's start time (Unix epoch seconds).
/// Returns None if sysinfo can't read our own process for any reason.
pub(crate) fn current_process_start_time() -> Option<i64> {
    process_start_time(std::process::id() as i64)
}

/// Look up start time for an arbitrary PID. Returns None if the
/// process doesn't exist or sysinfo can't read it.
fn process_start_time(pid: i64) -> Option<i64> {
    if pid <= 0 || pid > u32::MAX as i64 {
        return None;
    }
    let mut sys = System::new();
    let target = Pid::from_u32(pid as u32);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::new(),
    );
    sys.process(target).map(|p| p.start_time() as i64)
}

/// Liveness predicate passed to the store. Returns true if the recorded
/// holder is still the same live process.
///
/// Semantics:
///   * No process at `pid` now → false (dead, reclaimable).
///   * Process at `pid` exists but its start_time differs from the
///     recorded value → false (PID was reused, reclaimable).
///   * Process exists and start_time matches (or the recorded value
///     is None, legacy/missing) → true (still held).
fn is_same_alive(pid: i64, recorded_started_at: Option<i64>) -> bool {
    let Some(current_started) = process_start_time(pid) else {
        return false;
    };
    match recorded_started_at {
        Some(rec) => rec == current_started,
        None => true, // legacy row written before this column existed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_time_for_self_is_positive() {
        let t = current_process_start_time();
        assert!(t.is_some());
        assert!(t.unwrap() > 0);
    }

    #[test]
    fn process_start_time_for_dead_pid_is_none() {
        // PID 1_999_999_999 is essentially never assigned on a normal box.
        assert!(process_start_time(1_999_999_999).is_none());
    }

    #[test]
    fn process_start_time_rejects_invalid() {
        assert!(process_start_time(0).is_none());
        assert!(process_start_time(-1).is_none());
    }

    #[test]
    fn is_same_alive_self_with_matching_start_time() {
        let pid = std::process::id() as i64;
        let started = current_process_start_time().unwrap();
        assert!(is_same_alive(pid, Some(started)));
    }

    #[test]
    fn is_same_alive_self_with_mismatched_start_time_is_false() {
        let pid = std::process::id() as i64;
        // Pretend we recorded a different start time → treat as PID reuse.
        assert!(!is_same_alive(pid, Some(0)));
    }

    #[test]
    fn is_same_alive_dead_pid_is_false() {
        assert!(!is_same_alive(1_999_999_999, Some(12345)));
    }

    #[test]
    fn is_same_alive_with_missing_recorded_falls_through_to_pid_alive() {
        let pid = std::process::id() as i64;
        // Legacy row (None recorded) — fall back to "process exists at this pid".
        assert!(is_same_alive(pid, None));
        assert!(!is_same_alive(1_999_999_999, None));
    }
}
