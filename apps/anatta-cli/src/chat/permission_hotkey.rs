//! Shift+Tab keybinding for permission-level cycling.
//!
//! rustyline doesn't natively support "return a custom outcome from
//! readline." We approximate it: a `ConditionalEventHandler` sets a
//! shared `Arc<AtomicBool>` flag when Shift+Tab is pressed, then
//! returns `Cmd::AcceptLine` so readline returns. The caller checks
//! the flag and treats the read as a cycle event instead of a prompt.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, RepeatCount};

#[derive(Clone, Default)]
pub(crate) struct CyclePermissionFlag {
    inner: Arc<AtomicBool>,
}

impl CyclePermissionFlag {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Was the flag set since the last `take()`?
    pub(crate) fn take(&self) -> bool {
        self.inner.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn handler(&self) -> CyclePermissionHandler {
        CyclePermissionHandler {
            flag: self.inner.clone(),
        }
    }
}

pub(crate) struct CyclePermissionHandler {
    flag: Arc<AtomicBool>,
}

impl ConditionalEventHandler for CyclePermissionHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        self.flag.store(true, Ordering::SeqCst);
        Some(Cmd::AcceptLine)
    }
}
