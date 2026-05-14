# Claude Interactive PTY Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third backend session shape — `ClaudeInteractive` — that runs the real interactive `claude` TUI inside a pseudo-terminal, drives prompts via the master, captures structured events by tailing the session JSONL, and never shows the TUI to the user.

**Architecture:** The PTY exists only because `claude` (without `-p`) refuses to start without a tty; we discard every byte it writes. The actual data path is `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`, which `claude` appends to in real time and which the existing `HistoryProjector` already knows how to parse. We pick the `session_id` UUID up front via `--session-id`, so file discovery is a single `Path::exists` poll rather than a directory watch. Prompts are written to the PTY master with bracketed-paste framing + CR. Turn boundary = the `AgentEventPayload::TurnCompleted` already emitted by `HistoryProjector` when claude writes its `system { subtype: "turn_duration" }` record. The new type mirrors `PersistentCodexSession`: `open()` once, many `send_turn` calls, `close()` at end.

**Tech Stack:** Rust (workspace edition), `tokio` (process / sync / fs / time / io-util), `portable-pty 0.9` (new dep; cross-platform PTY), existing `anatta-runtime::claude::history::ClaudeEvent` + `HistoryProjector`, existing `anatta-runtime::conversation::paths::working_jsonl_path`.

---

## Architecture Overview

```
┌─ ClaudeInteractiveSession ──────────────────────────────────────────────┐
│                                                                          │
│   session_id (chosen at open) ─► passed to claude as --session-id        │
│                                                                          │
│   ┌─ writer thread (std::thread) ──────┐                                 │
│   │  owns: Box<dyn Write + Send>       │ ◄── mpsc::Receiver<PtyCommand>  │
│   │  loop:                             │                                 │
│   │    match rx.blocking_recv() {      │                                 │
│   │      Write(bytes) => write+flush,  │                                 │
│   │      Shutdown     => drop+return,  │                                 │
│   │    }                               │                                 │
│   └────────────────────────────────────┘                                 │
│                                                                          │
│   ┌─ drain thread (std::thread) ───────┐                                 │
│   │  io::copy(pty_reader, io::sink())  │   discards all TUI bytes        │
│   └────────────────────────────────────┘                                 │
│                                                                          │
│   ┌─ tail task   (tokio) ──────────────┐                                 │
│   │  poll jsonl path for appended      │                                 │
│   │  lines; deserialize ClaudeEvent;   │                                 │
│   │  HistoryProjector::project →       │                                 │
│   │  dispatch to active_turn.events_tx │                                 │
│   │  close active turn on              │                                 │
│   │  AgentEventPayload::TurnCompleted  │                                 │
│   └────────────────────────────────────┘                                 │
│                                                                          │
│   public API:                                                            │
│     send_turn(&self, prompt)  →  InteractiveTurnHandle                   │
│                                  (mpsc::Receiver<AgentEvent>)            │
│     interrupt_handle()        →  cloneable handle that writes Ctrl-C     │
│     close(self)               →  EOF + wait + ExitInfo                   │
└──────────────────────────────────────────────────────────────────────────┘
```

`send_turn` sequence:
1. Refuse if `active_turn` is already set.
2. Generate channel `(events_tx, events_rx)`.
3. Push synthetic `SessionStarted` (mirrors codex + one-shot claude).
4. Install `ActiveTurn { events_tx }` under the mutex.
5. Send `PtyCommand::Write(encode_prompt(prompt))` to the writer thread.
6. Return `InteractiveTurnHandle { events_rx }`.

The tail task watches `active_turn`. When it sees `AgentEventPayload::TurnCompleted`, it dispatches that event and **then** clears `active_turn`, which drops `events_tx` and closes the channel naturally.

---

## File Structure

**Created:**

- `crates/anatta-runtime/src/spawn/claude_interactive.rs` — `ClaudeInteractiveLaunch`, `ClaudeInteractiveSession`, `ClaudeInteractiveInterruptHandle`, `InteractiveTurnHandle`, `PtyCommand`, drain / writer / tail tasks. All new spawn machinery for this mode lives here.
- `crates/anatta-runtime/tests/claude_interactive_unit.rs` — unit tests for `encode_prompt` and the in-process tail-task driven against a synthetic JSONL file.

**Modified:**

- `crates/anatta-runtime/Cargo.toml` — add `portable-pty` and `uuid` (if not already in deps) under the `spawn` feature.
- `crates/anatta-runtime/src/spawn/mod.rs` — `mod claude_interactive`, re-export the new public types.
- `crates/anatta-runtime/src/spawn/session.rs` — add `BackendLaunch::ClaudeInteractive`, `Session::ClaudeInteractive`, and a third `TurnEventsInner::ClaudeInteractive` variant; update every match.
- `crates/anatta-runtime/src/conversation/paths.rs` — add `wait_for_jsonl(path, timeout)` poller (lives next to the existing path helpers).
- `crates/anatta-runtime/tests/spawn_e2e.rs` — add ignored real-claude tests for the interactive path.

Decomposition rationale: one new module covers spawn / writer / tail / lifecycle because they share private state (`active_turn`, `pty_tx`, `child`). Path helpers stay with their siblings in `conversation/paths.rs`. Tests live in `tests/` to match the project's convention.

---

## Task 1: Add `portable-pty` dependency and empty module

**Files:**
- Modify: `crates/anatta-runtime/Cargo.toml:11-30`
- Create: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/src/spawn/mod.rs:25-37`

- [ ] **Step 1: Add the dependency under the `spawn` feature**

In `crates/anatta-runtime/Cargo.toml`, extend the `spawn` feature to gate a new optional dep, and add the dep itself. Final shape of the two edits:

```toml
[features]
spawn = [
    "dep:tokio",
    "dep:async-trait",
    "dep:portable-pty",
    "dep:uuid",
]

[dependencies]
# ... existing entries unchanged ...
portable-pty = { version = "0.9", optional = true }
uuid         = { version = "1",   features = ["v4"], optional = true }
```

If `uuid` already exists as a workspace dep, use `uuid = { workspace = true, features = ["v4"], optional = true }` instead. Check with `grep '^uuid' Cargo.toml` at the repo root before adding.

- [ ] **Step 2: Create the empty module file**

Create `crates/anatta-runtime/src/spawn/claude_interactive.rs`:

```rust
//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).
//!
//! Mirrors the long-lived shape of
//! [`PersistentCodexSession`](crate::spawn::PersistentCodexSession): one
//! `open()`, many `send_turn()` calls, one `close()`.

// implementation lands in later tasks
```

- [ ] **Step 3: Wire the module into `spawn/mod.rs`**

In `crates/anatta-runtime/src/spawn/mod.rs`, replace the block currently at lines 25–37 with:

```rust
mod claude;
mod claude_interactive;
mod codex;
mod ids;
pub(crate) mod pipeline;
mod session;
mod stderr_buf;

pub use claude::ClaudeLaunch;
pub use codex::{CodexInterruptHandle, CodexLaunch, PersistentCodexSession, TurnHandle};
pub use ids::{ClaudeSessionId, CodexThreadId};
pub use session::{
    BackendKind, BackendLaunch, ClaudeSession, CodexSession, Session, SwapError, TurnEvents,
};
```

(No re-exports from `claude_interactive` yet — Task 3 adds them as the first symbol becomes public.)

- [ ] **Step 4: Verify Cargo.toml is well-formed**

Run: `cargo metadata --no-deps --format-version=1 -p anatta-runtime > /dev/null`

Expected: exit 0, no errors. (Do not run `cargo build` yet — the empty module compiles, but we'll verify build properly in Step 5.)

Run: `cargo check -p anatta-runtime --features spawn`

Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/Cargo.toml crates/anatta-runtime/src/spawn/mod.rs \
        crates/anatta-runtime/src/spawn/claude_interactive.rs
git commit -m "chore(runtime): scaffold spawn/claude_interactive module + portable-pty dep"
```

---

## Task 2: `wait_for_jsonl` path-discovery helper (TDD)

**Files:**
- Modify: `crates/anatta-runtime/src/conversation/paths.rs` (imports + new fn + tests)

- [ ] **Step 1: Write the failing tests**

Append to the existing `#[cfg(test)] mod tests` block in `crates/anatta-runtime/src/conversation/paths.rs`, before its closing `}`:

```rust
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

    #[tokio::test]
    async fn wait_for_jsonl_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("never.jsonl");
        let res = super::wait_for_jsonl(&path, std::time::Duration::from_millis(150)).await;
        assert!(res.is_err());
    }
```

Both `tokio` and `tempfile` are already in `[dev-dependencies]` of `crates/anatta-runtime/Cargo.toml`, so no extra wiring is needed.

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test -p anatta-runtime --lib paths::tests::wait_for_jsonl`

Expected: compile error — `wait_for_jsonl` doesn't exist.

- [ ] **Step 3: Implement `wait_for_jsonl`**

In `crates/anatta-runtime/src/conversation/paths.rs`, change the top import from:

```rust
use std::path::{Path, PathBuf};
```

to:

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;
```

Then append above the `#[cfg(test)] mod tests` block:

```rust
/// Poll `path` every 25 ms until it exists or `timeout` elapses.
///
/// Used by the interactive PTY spawn flow to know when claude has
/// actually created its session JSONL — at which point claude is past
/// startup and ready to receive a prompt over the PTY master.
///
/// Returns `Ok(())` as soon as the file exists, `Err` on timeout.
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
```

`paths.rs` currently has no `tokio` import in non-test code, but `tokio::time` will only be referenced here. The `anatta-runtime` crate already pulls in `tokio` under the `spawn` feature. Since `paths.rs` is in `conversation`, which is a non-feature-gated module, the new function needs to compile regardless of feature. Wrap it in a feature gate:

```rust
#[cfg(feature = "spawn")]
pub async fn wait_for_jsonl(path: &Path, timeout: Duration) -> std::io::Result<()> {
    // body as above
}
```

And gate the three new tests similarly:

```rust
    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_returns_immediately_when_present() { /* ... */ }
    // (same for the other two)
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p anatta-runtime --features spawn --lib paths::tests::wait_for_jsonl`

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/conversation/paths.rs
git commit -m "feat(runtime): wait_for_jsonl helper for interactive session discovery"
```

---

## Task 3: `encode_prompt` bracketed-paste encoder (TDD)

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/src/spawn/mod.rs` (test re-export)
- Create: `crates/anatta-runtime/tests/claude_interactive_unit.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/anatta-runtime/tests/claude_interactive_unit.rs`:

```rust
//! In-process unit tests for the interactive PTY backend.
//!
//! Real-PTY spawn tests live in `spawn_e2e.rs` (ignored; hits the
//! user's installed claude binary). These tests stay process-local and
//! run every CI build.

#![cfg(feature = "spawn")]

use anatta_runtime::spawn::encode_prompt_for_test;

#[test]
fn encode_prompt_wraps_in_bracketed_paste_and_terminates_with_cr() {
    let bytes = encode_prompt_for_test("Say only OK");
    assert_eq!(
        bytes,
        b"\x1b[200~Say only OK\x1b[201~\r".to_vec(),
        "expected bracketed-paste start, prompt, bracketed-paste end, CR",
    );
}

#[test]
fn encode_prompt_passes_through_newlines_inside_paste_bracket() {
    let bytes = encode_prompt_for_test("line one\nline two");
    assert_eq!(
        bytes,
        b"\x1b[200~line one\nline two\x1b[201~\r".to_vec(),
    );
}
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test -p anatta-runtime --features spawn --test claude_interactive_unit`

Expected: compile error — `encode_prompt_for_test` doesn't exist.

- [ ] **Step 3: Implement `encode_prompt` and the test-only re-export**

Replace the contents of `crates/anatta-runtime/src/spawn/claude_interactive.rs` with:

```rust
//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

/// Wrap `prompt` in xterm bracketed-paste escape sequences and terminate
/// with a CR (which claude's input handler interprets as "submit").
///
/// Bracketed paste tells claude these bytes are pasted content, not
/// typed — which preserves embedded newlines as literal newlines instead
/// of treating them as submit keystrokes mid-prompt.
pub(crate) fn encode_prompt(prompt: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prompt.len() + 8);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(prompt.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

#[doc(hidden)]
pub fn encode_prompt_for_test(prompt: &str) -> Vec<u8> {
    encode_prompt(prompt)
}
```

Then add the test-only re-export to `crates/anatta-runtime/src/spawn/mod.rs`. Below the existing `pub use` lines:

```rust
pub use claude_interactive::encode_prompt_for_test;
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p anatta-runtime --features spawn --test claude_interactive_unit`

Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        crates/anatta-runtime/src/spawn/mod.rs \
        crates/anatta-runtime/tests/claude_interactive_unit.rs
git commit -m "feat(runtime): encode_prompt — bracketed-paste prompt encoder"
```

---

## Task 4: In-process JSONL tail task (TDD)

The tail loop reads appended JSONL lines, deserializes them as `ClaudeEvent`, runs `HistoryProjector::project`, and pushes resulting `AgentEvent`s into a channel. It tracks file position across iterations so re-reads after EOF only see new bytes.

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/src/spawn/mod.rs`
- Modify: `crates/anatta-runtime/tests/claude_interactive_unit.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/anatta-runtime/tests/claude_interactive_unit.rs`:

```rust
use anatta_core::AgentEventPayload;
use anatta_runtime::spawn::run_tail_for_test;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn tail_emits_assistant_text_then_turn_completed_then_closes_on_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("session.jsonl");
    std::fs::write(&path, "").unwrap();

    let (tx, mut rx) = mpsc::channel(16);
    let path_for_task = path.clone();
    let handle = tokio::spawn(async move {
        run_tail_for_test(path_for_task, tx, "sess-1".to_owned()).await
    });

    let assistant_line = r#"{"type":"assistant","uuid":"u1","parentUuid":null,"sessionId":"sess-1","timestamp":"2026-05-14T00:00:00Z","cwd":"/tmp","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","message":{"id":"m1","model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"text","text":"OK"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#;
    let turn_done_line = r#"{"type":"system","uuid":"u2","parentUuid":"u1","sessionId":"sess-1","timestamp":"2026-05-14T00:00:01Z","cwd":"/tmp","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","subtype":"turn_duration","durationMs":1234,"messageCount":2}"#;

    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{assistant_line}").unwrap();
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{turn_done_line}").unwrap();
    }

    let mut payloads = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("tail produced events within 2s")
    {
        payloads.push(ev.payload);
    }

    assert!(matches!(payloads.first().unwrap(), AgentEventPayload::AssistantText { .. }));
    assert!(matches!(payloads.last().unwrap(), AgentEventPayload::TurnCompleted { .. }));
    handle.await.unwrap();
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p anatta-runtime --features spawn --test claude_interactive_unit tail_emits`

Expected: compile error — `run_tail_for_test` doesn't exist.

- [ ] **Step 3: Implement the tail loop and expose the test entry**

Replace `crates/anatta-runtime/src/spawn/claude_interactive.rs` with:

```rust
//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

use std::path::PathBuf;
use std::time::Duration;

use anatta_core::{AgentEvent, AgentEventPayload, ProjectionContext, Projector};
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::mpsc;

use crate::claude::history::ClaudeEvent;
use crate::claude::HistoryProjector;

// ──────────────────────────────────────────────────────────────────────
// Prompt encoding
// ──────────────────────────────────────────────────────────────────────

pub(crate) fn encode_prompt(prompt: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prompt.len() + 8);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(prompt.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

#[doc(hidden)]
pub fn encode_prompt_for_test(prompt: &str) -> Vec<u8> {
    encode_prompt(prompt)
}

// ──────────────────────────────────────────────────────────────────────
// JSONL tail
// ──────────────────────────────────────────────────────────────────────

/// Tail a claude session JSONL: read appended lines, parse as
/// `ClaudeEvent`, project to `AgentEvent`, push into `events_tx`. Close
/// the channel (return) when an `AgentEventPayload::TurnCompleted` is
/// observed OR when `events_tx` is dropped by the consumer.
///
/// Polling interval: 25 ms — cheap (the file is local and kernel-cached)
/// and fast enough that turn boundaries feel immediate. We re-open the
/// file each tick rather than holding it open while idle.
async fn run_tail(
    path: PathBuf,
    events_tx: mpsc::Sender<AgentEvent>,
    session_id: String,
) {
    let mut projector = HistoryProjector::new();
    let mut byte_offset: u64 = 0;
    let mut line_buf = String::new();
    let interval = Duration::from_millis(25);

    loop {
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => {
                tokio::time::sleep(interval).await;
                continue;
            }
        };
        let mut reader = BufReader::new(file);
        if reader.seek(std::io::SeekFrom::Start(byte_offset)).await.is_err() {
            tokio::time::sleep(interval).await;
            continue;
        }

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break, // EOF — fall through to sleep + reopen
                Ok(n) => {
                    // A line without trailing '\n' is partial — back up
                    // and re-read it next tick when the rest arrives.
                    if !line_buf.ends_with('\n') {
                        break;
                    }
                    byte_offset += n as u64;
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let raw: ClaudeEvent = match serde_json::from_str(trimmed) {
                        Ok(r) => r,
                        Err(_) => continue, // skip malformed; parser fixtures own coverage
                    };
                    let ctx = ProjectionContext {
                        session_id: session_id.clone(),
                        received_at: Utc::now(),
                    };
                    for ev in projector.project(&raw, &ctx) {
                        let is_completion =
                            matches!(ev.payload, AgentEventPayload::TurnCompleted { .. });
                        if events_tx.send(ev).await.is_err() {
                            return; // consumer dropped
                        }
                        if is_completion {
                            return; // turn done — close the channel
                        }
                    }
                }
                Err(_) => break,
            }
        }

        tokio::time::sleep(interval).await;
    }
}

#[doc(hidden)]
pub async fn run_tail_for_test(
    path: PathBuf,
    events_tx: mpsc::Sender<AgentEvent>,
    session_id: String,
) {
    run_tail(path, events_tx, session_id).await
}
```

Update `crates/anatta-runtime/src/spawn/mod.rs` re-exports:

```rust
pub use claude_interactive::{encode_prompt_for_test, run_tail_for_test};
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p anatta-runtime --features spawn --test claude_interactive_unit`

Expected: 3 tests pass total (2 from Task 3 + 1 new).

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        crates/anatta-runtime/src/spawn/mod.rs \
        crates/anatta-runtime/tests/claude_interactive_unit.rs
git commit -m "feat(runtime): JSONL tail task — appended lines → AgentEvent stream"
```

---

## Task 5: `ClaudeInteractiveLaunch` + `ClaudeInteractiveSession::open()`

This task adds: PTY allocation, child spawn with the right argv, drain thread, writer thread, JSONL discovery, and a "persistent tail" variant of `run_tail` that routes events to whatever turn is currently active. No `send_turn` yet (that's Task 6) — just `open()` returning a session struct.

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/src/spawn/mod.rs`

- [ ] **Step 1: Append the public types and the spawn implementation**

Append to `crates/anatta-runtime/src/spawn/claude_interactive.rs` (after `run_tail_for_test`):

```rust
// ──────────────────────────────────────────────────────────────────────
// Public configuration
// ──────────────────────────────────────────────────────────────────────

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anatta_core::{AgentEventEnvelope, Backend};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::Mutex;

use crate::conversation::paths::{wait_for_jsonl, working_jsonl_path};
use crate::profile::ClaudeProfile;
use crate::spawn::stderr_buf;
use crate::spawn::{ClaudeSessionId, ExitInfo, SpawnError};

const PTY_ROWS: u16 = 50;
const PTY_COLS: u16 = 200;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Configuration for spawning an interactive (PTY-wrapped) claude session.
#[derive(Debug, Clone)]
pub struct ClaudeInteractiveLaunch {
    pub profile: ClaudeProfile,
    /// MUST be canonicalized (e.g. `/tmp` → `/private/tmp` on macOS)
    /// because we derive the JSONL path from it via `working_jsonl_path`
    /// and claude does the same canonicalization internally.
    pub cwd: PathBuf,
    /// `Some(id)` → `--resume <id>`. `None` → fresh session; a new UUID
    /// will be chosen at `open()` and passed via `--session-id`.
    pub resume: Option<ClaudeSessionId>,
    pub binary_path: PathBuf,
    pub provider: Option<crate::profile::ProviderEnv>,
    /// Optional model override (passed as `--model <name>`).
    pub model: Option<String>,
    /// If true, pass `--bare` for a minimal child environment
    /// (no hooks / LSP / plugin sync / CLAUDE.md auto-discovery /
    /// keychain reads). Defaults to true — anatta owns the environment.
    pub bare: bool,
}

// ──────────────────────────────────────────────────────────────────────
// Internal types
// ──────────────────────────────────────────────────────────────────────

enum PtyCommand {
    Write(Vec<u8>),
    Shutdown,
}

struct ActiveTurn {
    events_tx: mpsc::Sender<AgentEvent>,
}

// ──────────────────────────────────────────────────────────────────────
// Session
// ──────────────────────────────────────────────────────────────────────

/// Long-lived interactive `claude` connection.
pub struct ClaudeInteractiveSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[allow(dead_code)] // held so resize(...) can be added later
    master: Box<dyn MasterPty + Send>,
    session_id: ClaudeSessionId,
    pty_tx: mpsc::Sender<PtyCommand>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    stderr: stderr_buf::Handle, // empty for PTY (combined stdio); kept for ExitInfo shape
    started_at: Instant,
    events_emitted: Arc<AtomicU64>,
}

/// Cloneable handle for interrupting the active turn from outside — see
/// [`ClaudeInteractiveSession::interrupt_handle`].
#[derive(Clone)]
pub struct ClaudeInteractiveInterruptHandle {
    pty_tx: mpsc::Sender<PtyCommand>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
}

impl ClaudeInteractiveSession {
    pub async fn open(launch: ClaudeInteractiveLaunch) -> Result<Self, SpawnError> {
        if !launch.binary_path.exists() {
            return Err(SpawnError::BinaryNotFound(launch.binary_path.clone()));
        }
        if !launch.profile.path.is_dir() {
            return Err(SpawnError::ProfilePathInvalid(launch.profile.path.clone()));
        }

        // Pick session id up front so we know the JSONL path without
        // having to discover the filename later.
        let session_id = launch
            .resume
            .clone()
            .unwrap_or_else(|| ClaudeSessionId::new(uuid::Uuid::new_v4().to_string()));

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("openpty: {e}"))))?;

        let mut cmd = CommandBuilder::new(&launch.binary_path);
        cmd.env(
            "CLAUDE_CONFIG_DIR",
            launch.profile.path.to_string_lossy().as_ref(),
        );
        cmd.env(
            "TERM",
            std::env::var("TERM").as_deref().unwrap_or("xterm-256color"),
        );
        if let Some(env) = &launch.provider {
            for (k, v) in &env.vars {
                cmd.env(k, v);
            }
        }
        cmd.cwd(launch.cwd.as_os_str());
        cmd.arg("--session-id");
        cmd.arg(session_id.as_str());
        cmd.arg("--permission-mode");
        cmd.arg("bypassPermissions");
        if launch.bare {
            cmd.arg("--bare");
        }
        if let Some(m) = &launch.model {
            cmd.arg("--model");
            cmd.arg(m);
        }
        if launch.resume.is_some() {
            cmd.arg("--resume");
            cmd.arg(session_id.as_str());
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("spawn: {e}"))))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("clone_reader: {e}"))))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!("take_writer: {e}"))))?;

        // Drain thread: must read continuously or the kernel PTY buffer
        // fills, claude blocks on its next write, and the JSONL stops
        // being appended.
        std::thread::Builder::new()
            .name("claude-pty-drain".into())
            .spawn(move || {
                let _ = std::io::copy(&mut reader, &mut std::io::sink());
            })
            .map_err(SpawnError::Io)?;

        // Writer thread: owns the sync `Write`, pulls commands off a
        // tokio mpsc. Dedicated OS thread because portable-pty's writer
        // is blocking.
        let (pty_tx, mut pty_rx) = mpsc::channel::<PtyCommand>(8);
        std::thread::Builder::new()
            .name("claude-pty-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Some(cmd) = pty_rx.blocking_recv() {
                    match cmd {
                        PtyCommand::Write(bytes) => {
                            if std::io::Write::write_all(&mut writer, &bytes).is_err() {
                                break;
                            }
                            let _ = std::io::Write::flush(&mut writer);
                        }
                        PtyCommand::Shutdown => break,
                    }
                }
                // Dropping `writer` closes the master end of the PTY,
                // which sends EOF / SIGHUP to the child.
            })
            .map_err(SpawnError::Io)?;

        // Wait for the JSONL file to appear — its presence means
        // claude is past startup and the input handler is alive.
        let cwd_str = launch
            .cwd
            .to_str()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;
        let jsonl = working_jsonl_path(&launch.profile.path, cwd_str, session_id.as_str());
        wait_for_jsonl(&jsonl, STARTUP_TIMEOUT)
            .await
            .map_err(|e| SpawnError::Io(std::io::Error::other(format!(
                "claude did not write its session JSONL at {} within {:?}: {}",
                jsonl.display(),
                STARTUP_TIMEOUT,
                e,
            ))))?;

        let active_turn: Arc<Mutex<Option<ActiveTurn>>> = Arc::new(Mutex::new(None));
        let events_emitted = Arc::new(AtomicU64::new(0));
        let stderr_handle = stderr_buf::Handle::new();

        // Tail task: dispatches projected events to the currently-
        // active turn (if any).
        let active_for_tail = active_turn.clone();
        let counter_for_tail = events_emitted.clone();
        let session_id_for_tail = session_id.as_str().to_owned();
        tokio::spawn(async move {
            persistent_tail_loop(
                jsonl,
                active_for_tail,
                counter_for_tail,
                session_id_for_tail,
            )
            .await;
        });

        Ok(Self {
            child,
            master: pair.master,
            session_id,
            pty_tx,
            active_turn,
            stderr: stderr_handle,
            started_at: Instant::now(),
            events_emitted,
        })
    }

    pub fn session_id(&self) -> &str {
        self.session_id.as_str()
    }

    pub async fn is_idle(&self) -> bool {
        self.active_turn.lock().await.is_none()
    }

    pub fn interrupt_handle(&self) -> ClaudeInteractiveInterruptHandle {
        ClaudeInteractiveInterruptHandle {
            pty_tx: self.pty_tx.clone(),
            active_turn: self.active_turn.clone(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Persistent tail
// ──────────────────────────────────────────────────────────────────────

/// Like `run_tail`, but routes events to the currently-active turn (if
/// any) instead of a single fixed channel. Closes the active turn on
/// `AgentEventPayload::TurnCompleted`.
async fn persistent_tail_loop(
    path: PathBuf,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    counter: Arc<AtomicU64>,
    session_id: String,
) {
    let mut projector = HistoryProjector::new();
    let mut byte_offset: u64 = 0;
    let mut line_buf = String::new();
    let interval = Duration::from_millis(25);

    loop {
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => {
                tokio::time::sleep(interval).await;
                continue;
            }
        };
        let mut reader = BufReader::new(file);
        if reader.seek(std::io::SeekFrom::Start(byte_offset)).await.is_err() {
            tokio::time::sleep(interval).await;
            continue;
        }

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if !line_buf.ends_with('\n') {
                        break;
                    }
                    byte_offset += n as u64;
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let raw: ClaudeEvent = match serde_json::from_str(trimmed) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let ctx = ProjectionContext {
                        session_id: session_id.clone(),
                        received_at: Utc::now(),
                    };
                    let evs = projector.project(&raw, &ctx);

                    let mut at = active.lock().await;
                    let Some(turn) = at.as_mut() else {
                        // No active turn — nothing to dispatch to. We
                        // still advanced `byte_offset` so we don't
                        // re-read these lines next tick.
                        continue;
                    };
                    let mut completed = false;
                    for ev in evs {
                        completed |= matches!(ev.payload, AgentEventPayload::TurnCompleted { .. });
                        if turn.events_tx.send(ev).await.is_err() {
                            *at = None;
                            break;
                        }
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    if completed {
                        *at = None;
                    }
                }
                Err(_) => break,
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

async fn push_synthetic_session_started(
    events_tx: &mpsc::Sender<AgentEvent>,
    counter: &AtomicU64,
    session_id: &str,
    cwd_str: &str,
) -> Result<(), SpawnError> {
    let ev = AgentEvent {
        envelope: AgentEventEnvelope {
            session_id: session_id.to_owned(),
            timestamp: Utc::now(),
            backend: Backend::Claude,
            raw_uuid: None,
            parent_tool_use_id: None,
        },
        payload: AgentEventPayload::SessionStarted {
            cwd: cwd_str.to_owned(),
            model: String::new(),
            tools_available: Vec::new(),
        },
    };
    events_tx
        .send(ev)
        .await
        .map_err(|_| SpawnError::Io(std::io::Error::other("consumer channel closed")))?;
    counter.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
```

Update `crates/anatta-runtime/src/spawn/mod.rs` to expose the new public types alongside the existing test exports:

```rust
pub use claude_interactive::{
    encode_prompt_for_test, run_tail_for_test, ClaudeInteractiveInterruptHandle,
    ClaudeInteractiveLaunch, ClaudeInteractiveSession,
};
```

- [ ] **Step 2: Compile-check**

Run: `cargo build -p anatta-runtime --features spawn`

Expected: clean build. If `portable-pty` fails to build, follow its README (on macOS / Linux it should compile with no extra setup).

- [ ] **Step 3: Run the existing test suite**

Run: `cargo test -p anatta-runtime --features spawn`

Expected: every prior test still passes. No new tests run here — `open()` cannot be unit-tested without a real claude binary, which Task 6's ignored E2E covers.

- [ ] **Step 4: Verify the public surface compiles for external consumers**

Run: `cargo check -p anatta-runtime --features spawn --tests`

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        crates/anatta-runtime/src/spawn/mod.rs
git commit -m "feat(runtime): ClaudeInteractiveSession::open — PTY spawn + JSONL discovery"
```

---

## Task 6: `send_turn` returning a `TurnHandle` (TDD via real-claude E2E)

The send path is too thin to unit-test meaningfully (the body really is "encode prompt → push bytes onto the PTY writer channel" and Task 3 already covers `encode_prompt`). The Task 4 unit test already covers the tail path. So this task uses an `#[ignore]` real-claude E2E test as verification.

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/src/spawn/mod.rs`
- Modify: `crates/anatta-runtime/tests/spawn_e2e.rs`

- [ ] **Step 1: Write the failing E2E test**

Append to `crates/anatta-runtime/tests/spawn_e2e.rs`:

```rust
#[tokio::test]
#[ignore = "real claude API call; requires logged-in ~/.claude"]
async fn launch_real_claude_interactive_emits_session_started_assistant_completion() {
    use anatta_runtime::spawn::{ClaudeInteractiveLaunch, ClaudeInteractiveSession};

    let bin = locate_binary("claude").expect("claude binary not on PATH");
    let claude_dir = home().join(".claude");
    assert!(claude_dir.is_dir(), "no ~/.claude found; log in via `claude /login` first");

    let profile = ClaudeProfile {
        id: ClaudeProfileId::new(),
        path: claude_dir,
    };
    let cwd_tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize: macOS resolves /tmp → /private/tmp; claude's JSONL
    // path uses the canonical form.
    let cwd = std::fs::canonicalize(cwd_tmp.path()).expect("canonicalize");

    let launch = ClaudeInteractiveLaunch {
        profile,
        cwd,
        resume: None,
        binary_path: bin,
        provider: None,
        model: None,
        bare: true,
    };

    let session = ClaudeInteractiveSession::open(launch).await.expect("open");
    let session_id = session.session_id().to_owned();
    eprintln!("interactive session_id = {session_id}");
    assert!(!session_id.is_empty());

    let mut turn = session
        .send_turn("Say only OK and nothing else")
        .await
        .expect("send_turn");

    let mut saw_session_started = false;
    let mut saw_assistant_text = false;
    let mut saw_turn_completed = false;
    let mut all_text = String::new();
    while let Some(ev) = turn.events().recv().await {
        match &ev.payload {
            AgentEventPayload::SessionStarted { .. } => saw_session_started = true,
            AgentEventPayload::AssistantText { text } => {
                saw_assistant_text = true;
                all_text.push_str(text);
            }
            AgentEventPayload::TurnCompleted { .. } => saw_turn_completed = true,
            _ => {}
        }
    }
    assert!(saw_session_started, "no SessionStarted");
    assert!(saw_assistant_text, "no AssistantText");
    assert!(saw_turn_completed, "no TurnCompleted");
    assert!(
        all_text.to_ascii_lowercase().contains("ok"),
        "expected reply containing 'OK': {all_text:?}"
    );

    let exit = session.close().await.expect("close");
    eprintln!(
        "interactive exit code={:?} duration={:?} events={}",
        exit.exit_code, exit.duration, exit.events_emitted
    );
}
```

The test calls both `session.send_turn(...)` and `session.close().await` — neither exists yet. Task 6 adds `send_turn`; Task 8 adds `close`. We'll treat the test as a `--no-run` compile target at the end of Task 6 and the full run as the verification for Task 8.

- [ ] **Step 2: Run the E2E test to confirm it fails to compile**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e launch_real_claude_interactive -- --ignored`

Expected: compile error — `send_turn` and `close` don't exist on `ClaudeInteractiveSession`.

- [ ] **Step 3: Implement `send_turn` and the `InteractiveTurnHandle` type**

Append to `crates/anatta-runtime/src/spawn/claude_interactive.rs`:

```rust
/// Per-turn handle returned by [`ClaudeInteractiveSession::send_turn`].
///
/// Drain `events()` until it returns `None`; that signals end-of-turn.
pub struct InteractiveTurnHandle {
    events_rx: mpsc::Receiver<AgentEvent>,
}

impl InteractiveTurnHandle {
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events_rx
    }
}

impl ClaudeInteractiveSession {
    /// Send a new turn. Refuses if the previous turn is still active.
    pub async fn send_turn(&self, prompt: &str) -> Result<InteractiveTurnHandle, SpawnError> {
        let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);

        {
            let mut active = self.active_turn.lock().await;
            if active.is_some() {
                return Err(SpawnError::Io(std::io::Error::other(
                    "send_turn called while previous turn still active",
                )));
            }
            *active = Some(ActiveTurn {
                events_tx: events_tx.clone(),
            });
        }

        push_synthetic_session_started(
            &events_tx,
            &self.events_emitted,
            self.session_id.as_str(),
            "",
        )
        .await?;

        self.pty_tx
            .send(PtyCommand::Write(encode_prompt(prompt)))
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("pty writer task gone")))?;

        Ok(InteractiveTurnHandle { events_rx })
    }
}
```

Re-export `InteractiveTurnHandle` from `crates/anatta-runtime/src/spawn/mod.rs`:

```rust
pub use claude_interactive::{
    encode_prompt_for_test, run_tail_for_test, ClaudeInteractiveInterruptHandle,
    ClaudeInteractiveLaunch, ClaudeInteractiveSession, InteractiveTurnHandle,
};
```

- [ ] **Step 4: Confirm the E2E test compiles (still won't run; `close` missing)**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e --no-run`

Expected: compile error specifically on the `session.close()` line. Task 8 fixes that. Anything else failing is a Task-6 bug.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        crates/anatta-runtime/src/spawn/mod.rs \
        crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "feat(runtime): ClaudeInteractiveSession::send_turn over PTY"
```

---

## Task 7: `cancel` — Ctrl-C via PTY master + interrupt handle

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`
- Modify: `crates/anatta-runtime/tests/spawn_e2e.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/anatta-runtime/tests/spawn_e2e.rs`:

```rust
#[tokio::test]
#[ignore = "real claude API call; requires logged-in ~/.claude"]
async fn interactive_cancel_closes_turn_channel() {
    use anatta_runtime::spawn::{ClaudeInteractiveLaunch, ClaudeInteractiveSession};
    use std::time::Duration;

    let bin = locate_binary("claude").expect("claude binary not on PATH");
    let claude_dir = home().join(".claude");
    let profile = ClaudeProfile { id: ClaudeProfileId::new(), path: claude_dir };
    let cwd = std::fs::canonicalize(tempfile::tempdir().unwrap().path()).unwrap();

    let session = ClaudeInteractiveSession::open(ClaudeInteractiveLaunch {
        profile, cwd, resume: None, binary_path: bin, provider: None, model: None, bare: true,
    })
    .await
    .expect("open");

    let mut turn = session
        .send_turn("Count slowly from 1 to 100, one number per line, with a thoughtful sentence after each")
        .await
        .expect("send_turn");

    // Let some assistant output start, then cancel.
    let _ = tokio::time::timeout(Duration::from_secs(3), turn.events().recv()).await;
    session.interrupt_handle().interrupt().await.expect("interrupt");

    // Channel must close within a reasonable grace.
    let drain = async {
        while turn.events().recv().await.is_some() {}
    };
    tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .expect("turn channel did not close within 10s after interrupt");
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e interactive_cancel -- --ignored`

Expected: compile error — `ClaudeInteractiveInterruptHandle::interrupt` doesn't exist.

- [ ] **Step 3: Implement `interrupt`**

Append to `crates/anatta-runtime/src/spawn/claude_interactive.rs`:

```rust
impl ClaudeInteractiveInterruptHandle {
    /// Cancel the currently-active turn by sending Ctrl-C through the
    /// PTY. Idempotent — no-op if no turn is active or the session is
    /// closed. claude's TUI handles `\x03` as "interrupt current turn";
    /// the JSONL emits a `turn_duration` shortly after, which the tail
    /// task turns into `TurnCompleted` and the channel closes naturally.
    pub async fn interrupt(&self) -> Result<(), SpawnError> {
        {
            let active = self.active_turn.lock().await;
            if active.is_none() {
                return Ok(());
            }
        }
        self.pty_tx
            .send(PtyCommand::Write(vec![0x03]))
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("pty writer task gone")))
    }
}
```

- [ ] **Step 4: Run the test (real claude)**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e interactive_cancel -- --ignored --nocapture`

Expected: turn opens, some events flow, interrupt fires, channel closes within 10 s. If it hangs past 10 s, the most likely cause is that the prompt completed too quickly to interrupt — lengthen the prompt or shorten the initial wait.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "feat(runtime): ClaudeInteractiveInterruptHandle::interrupt via Ctrl-C"
```

---

## Task 8: `close` — graceful shutdown + ExitInfo

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`

The verification for this task is the Task 6 E2E test (which already calls `session.close().await.expect("close")` and ends by printing exit info). No new test code needed.

- [ ] **Step 1: Re-run the Task 6 test to confirm it still fails (now only at `close`)**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e launch_real_claude_interactive_emits -- --ignored`

Expected: compile error — `close` not defined on `ClaudeInteractiveSession`.

- [ ] **Step 2: Implement `close`**

Append to `crates/anatta-runtime/src/spawn/claude_interactive.rs`:

```rust
impl ClaudeInteractiveSession {
    /// Shut the session down: tell the writer thread to drop the master
    /// (which sends SIGHUP to the child), wait up to `CLOSE_GRACE` for
    /// natural exit, otherwise surface a timeout error. portable-pty's
    /// `Child::wait` is sync, so we drive it on a blocking task.
    pub async fn close(self) -> Result<ExitInfo, SpawnError> {
        let _ = self.pty_tx.send(PtyCommand::Shutdown).await;
        let started_at = self.started_at;
        let stderr = self.stderr;
        let events_emitted = self.events_emitted;
        let mut child = self.child;

        let wait_handle = tokio::task::spawn_blocking(move || child.wait());

        match tokio::time::timeout(CLOSE_GRACE, wait_handle).await {
            Ok(joined) => {
                let status = joined
                    .map_err(|e| SpawnError::Io(std::io::Error::other(e)))?
                    .map_err(|e| SpawnError::Io(std::io::Error::other(format!(
                        "child.wait: {e}"
                    ))))?;
                Ok(ExitInfo {
                    exit_code: Some(status.exit_code() as i32),
                    signal: None, // portable-pty does not uniformly expose signal info
                    duration: started_at.elapsed(),
                    stderr_tail: stderr.snapshot(),
                    events_emitted: events_emitted.load(Ordering::Relaxed),
                })
            }
            Err(_) => Err(SpawnError::Io(std::io::Error::other(
                "interactive claude did not exit within close grace",
            ))),
        }
    }
}
```

Note that `ExitInfo` has different field shapes on `cfg(unix)` vs not — check `crates/anatta-runtime/src/spawn/mod.rs:172-181`. If the struct uses `#[cfg(unix)] signal: ...`, the literal `signal: None` may not compile on non-unix builds. The safe form is:

```rust
                Ok(ExitInfo {
                    exit_code: Some(status.exit_code() as i32),
                    #[cfg(unix)]
                    signal: None,
                    #[cfg(not(unix))]
                    signal: None,
                    duration: started_at.elapsed(),
                    stderr_tail: stderr.snapshot(),
                    events_emitted: events_emitted.load(Ordering::Relaxed),
                })
```

If claude does not exit on PTY hangup in practice (some TUIs ignore SIGHUP), add an explicit `/exit\r` write before the `Shutdown`:

```rust
let _ = self.pty_tx.send(PtyCommand::Write(b"/exit\r".to_vec())).await;
let _ = self.pty_tx.send(PtyCommand::Shutdown).await;
```

Try without the `/exit` first; add it only if Step 4 times out.

- [ ] **Step 3: Compile-check**

Run: `cargo build -p anatta-runtime --features spawn --tests`

Expected: clean.

- [ ] **Step 4: Run the full E2E test**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e launch_real_claude_interactive_emits -- --ignored --nocapture`

Expected: every assertion passes; exit info printed at the end.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs
git commit -m "feat(runtime): ClaudeInteractiveSession::close — graceful PTY shutdown"
```

---

## Task 9: Wire `ClaudeInteractive` into `Session` / `BackendLaunch`

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/session.rs`

This task makes `ClaudeInteractiveSession` a peer of `ClaudeSession` and `CodexSession` so the chat REPL and one-shot send paths can pick interactive mode through the same enum.

- [ ] **Step 1: Update the `super::` import line**

In `crates/anatta-runtime/src/spawn/session.rs`, replace the existing `use super::{ ... }` block near the top with:

```rust
use super::{
    AgentSession, ClaudeInteractiveInterruptHandle, ClaudeInteractiveLaunch,
    ClaudeInteractiveSession, ClaudeLaunch, ClaudeSessionId, CodexInterruptHandle, CodexLaunch,
    CodexThreadId, ExitInfo, InteractiveTurnHandle, Launchable, PersistentCodexSession,
    SpawnError, TurnHandle,
};
```

- [ ] **Step 2: Add the `BackendLaunch::ClaudeInteractive` variant and update its helpers**

Replace the existing `BackendLaunch` enum + `impl BackendLaunch` block (currently around lines 49–78) with:

```rust
#[derive(Debug, Clone)]
pub enum BackendLaunch {
    Claude(ClaudeLaunch),
    ClaudeInteractive(ClaudeInteractiveLaunch),
    Codex(CodexLaunch),
}

impl BackendLaunch {
    pub fn kind(&self) -> BackendKind {
        match self {
            BackendLaunch::Claude(_) => BackendKind::Claude,
            BackendLaunch::ClaudeInteractive(_) => BackendKind::Claude,
            BackendLaunch::Codex(_) => BackendKind::Codex,
        }
    }

    pub fn cwd(&self) -> &std::path::Path {
        match self {
            BackendLaunch::Claude(l) => &l.cwd,
            BackendLaunch::ClaudeInteractive(l) => &l.cwd,
            BackendLaunch::Codex(l) => &l.cwd,
        }
    }

    pub fn resume_id(&self) -> Option<&str> {
        match self {
            BackendLaunch::Claude(l) => l.resume.as_ref().map(|r| r.as_str()),
            BackendLaunch::ClaudeInteractive(l) => l.resume.as_ref().map(|r| r.as_str()),
            BackendLaunch::Codex(l) => l.resume.as_ref().map(|r| r.as_str()),
        }
    }
}
```

- [ ] **Step 3: Add the `Session::ClaudeInteractive` variant and update every method**

Replace the existing `pub enum Session { ... }` + `impl Session { ... }` block with:

```rust
pub enum Session {
    Claude(ClaudeSession),
    ClaudeInteractive(ClaudeInteractiveSession),
    Codex(CodexSession),
}

impl Session {
    pub async fn open(launch: BackendLaunch) -> Result<Self, SpawnError> {
        match launch {
            BackendLaunch::Claude(l) => Ok(Session::Claude(ClaudeSession::open(l))),
            BackendLaunch::ClaudeInteractive(l) => {
                Ok(Session::ClaudeInteractive(ClaudeInteractiveSession::open(l).await?))
            }
            BackendLaunch::Codex(l) => Ok(Session::Codex(CodexSession::open(l).await?)),
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Session::Claude(_) => BackendKind::Claude,
            Session::ClaudeInteractive(_) => BackendKind::Claude,
            Session::Codex(_) => BackendKind::Codex,
        }
    }

    pub async fn is_idle(&self) -> bool {
        match self {
            Session::Claude(_) => true,
            Session::ClaudeInteractive(c) => c.is_idle().await,
            Session::Codex(c) => c.inner.is_idle().await,
        }
    }

    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Session::Claude(c) => c.thread_id.as_ref().map(|t| t.as_str()),
            Session::ClaudeInteractive(c) => Some(c.session_id()),
            Session::Codex(c) => Some(c.inner.thread_id()),
        }
    }

    pub async fn send_turn(&mut self, prompt: &str) -> Result<TurnEvents, SpawnError> {
        match self {
            Session::Claude(c) => c.send_turn(prompt).await,
            Session::ClaudeInteractive(c) => {
                let handle = c.send_turn(prompt).await?;
                let interrupt = c.interrupt_handle();
                Ok(TurnEvents {
                    inner: TurnEventsInner::ClaudeInteractive { handle, interrupt },
                    captured_exit: None,
                })
            }
            Session::Codex(c) => c.send_turn(prompt).await,
        }
    }

    pub async fn swap(&mut self, new_launch: BackendLaunch) -> Result<(), SwapError> {
        if !self.is_idle().await {
            return Err(SwapError::TurnActive);
        }
        let new_kind = new_launch.kind();
        let cur_kind = self.kind();

        let same_shape = matches!(
            (&*self, &new_launch),
            (Session::Claude(_), BackendLaunch::Claude(_))
            | (Session::ClaudeInteractive(_), BackendLaunch::ClaudeInteractive(_))
            | (Session::Codex(_), BackendLaunch::Codex(_))
        );

        if cur_kind == new_kind && same_shape {
            // Within-shape same-backend swap (e.g. claude → claude
            // template replace, codex → codex re-open).
            match (self, new_launch) {
                (Session::Claude(c), BackendLaunch::Claude(l)) => {
                    c.swap(l);
                    return Ok(());
                }
                (Session::ClaudeInteractive(_), BackendLaunch::ClaudeInteractive(_)) => {
                    // No template-only swap for interactive: env is
                    // baked into the live PTY child. Fall through to
                    // close-and-reopen below.
                }
                (Session::Codex(c), BackendLaunch::Codex(l)) => {
                    c.swap(l).await.map_err(SwapError::Spawn)?;
                    return Ok(());
                }
                _ => unreachable!("same_shape match exhausted"),
            }
        }

        // Cross-backend or cross-shape swap. Open new first, then close
        // old, so a failed open leaves the existing session intact.
        let new_session = Self::open(new_launch).await.map_err(SwapError::Spawn)?;
        let old_session = std::mem::replace(self, new_session);
        match old_session {
            Session::Claude(_) => {}
            Session::ClaudeInteractive(c) => {
                let _ = c.close().await;
            }
            Session::Codex(c) => {
                let _ = c.close().await;
            }
        }
        Ok(())
    }

    pub async fn close(self) -> Result<Option<ExitInfo>, SpawnError> {
        match self {
            Session::Claude(_) => Ok(None),
            Session::ClaudeInteractive(c) => Ok(Some(c.close().await?)),
            Session::Codex(c) => Ok(Some(c.close().await?)),
        }
    }
}
```

- [ ] **Step 4: Add the `TurnEventsInner::ClaudeInteractive` variant + update `TurnEvents` methods**

Still in `session.rs`, replace the `enum TurnEventsInner` definition and the relevant `TurnEvents` methods with:

```rust
enum TurnEventsInner {
    Claude(AgentSession),
    ClaudeInteractive {
        handle: InteractiveTurnHandle,
        interrupt: ClaudeInteractiveInterruptHandle,
    },
    Codex {
        handle: TurnHandle,
        interrupt: CodexInterruptHandle,
    },
}

impl TurnEvents {
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        match &mut self.inner {
            TurnEventsInner::Claude(s) => s.events().recv().await,
            TurnEventsInner::ClaudeInteractive { handle, .. } => handle.events().recv().await,
            TurnEventsInner::Codex { handle, .. } => handle.events().recv().await,
        }
    }

    pub async fn cancel(&mut self) -> Result<(), SpawnError> {
        if self.captured_exit.is_some() {
            return Ok(());
        }
        match &mut self.inner {
            TurnEventsInner::Claude(s) => {
                let exit = s.cancel_mut().await?;
                self.captured_exit = Some(exit);
                Ok(())
            }
            TurnEventsInner::ClaudeInteractive { interrupt, .. } => interrupt.interrupt().await,
            TurnEventsInner::Codex { interrupt, .. } => interrupt.interrupt().await,
        }
    }

    pub async fn finalize(mut self) -> Result<Option<ExitInfo>, SpawnError> {
        if let Some(exit) = self.captured_exit.take() {
            return Ok(Some(exit));
        }
        match self.inner {
            TurnEventsInner::Claude(s) => Ok(Some(s.wait().await?)),
            TurnEventsInner::ClaudeInteractive { .. } => Ok(None),
            TurnEventsInner::Codex { .. } => Ok(None),
        }
    }
}
```

- [ ] **Step 5: Run the full test suite + commit**

Run: `cargo test -p anatta-runtime --features spawn`

Expected: every existing test still passes; nothing broken by the `Session` refactor. The ignored E2E tests are unaffected here.

Then commit:

```bash
git add crates/anatta-runtime/src/spawn/session.rs
git commit -m "feat(runtime): wire ClaudeInteractive into Session/BackendLaunch/TurnEvents"
```

---

## Task 10: Module-doc pass + final verification

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/mod.rs:1-23`

- [ ] **Step 1: Update the top-of-file module doc**

In `crates/anatta-runtime/src/spawn/mod.rs`, replace the existing `//!` block at lines 1–23 with:

```rust
//! Backend subprocess supervision.
//!
//! Three session shapes:
//!
//! * [`ClaudeLaunch`] — per-turn `claude --print --output-format
//!   stream-json` spawn. Cold but simple; consumed by one-shot `anatta
//!   send`. Each turn is a fresh child reading prompt from argv,
//!   emitting structured stream-json on stdout.
//! * [`ClaudeInteractiveLaunch`] — long-lived interactive `claude` (no
//!   `--print`) inside a PTY. Prompts written to the master with
//!   bracketed-paste framing; structured events tailed from
//!   `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`;
//!   TUI render bytes discarded. Use when warm-process latency matters
//!   more than the simplicity of `--print`.
//! * [`CodexLaunch`] — codex `app-server` (JSON-RPC 2.0 over stdio).
//!   Either one-shot ([`Launchable`]) or persistent
//!   ([`PersistentCodexSession`]).
//!
//! All three converge on the [`AgentSession`] / [`Session`] /
//! [`TurnEvents`] consumer contract. `launch()` blocks until the first
//! event arrives, extracting `session_id` from it (claude `--print`:
//! `system/init`; claude interactive: chosen up front via `--session-id`;
//! codex: `thread.started`). The first event is also forwarded to the
//! consumer-facing channel — nothing is silently consumed.
```

- [ ] **Step 2: Build docs and verify intra-doc links resolve**

Run: `cargo doc -p anatta-runtime --features spawn --no-deps 2>&1 | tee /tmp/doc-out`

Expected: builds clean. Search the output: `grep -i 'warning\|broken' /tmp/doc-out` — fix any broken `[Foo]` links.

- [ ] **Step 3: Final non-ignored test pass**

Run: `cargo test -p anatta-runtime --features spawn`

Expected: full pass.

- [ ] **Step 4: Final ignored-test pass (real claude — optional but recommended)**

If you have a logged-in `~/.claude`, re-run the two E2E tests:

```
cargo test -p anatta-runtime --features spawn --test spawn_e2e -- \
    --ignored --nocapture launch_real_claude_interactive interactive_cancel
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/mod.rs
git commit -m "docs(runtime): ClaudeInteractive session shape in spawn module docs"
```

---

## Risks & follow-ups (deferred; not part of this plan)

1. **Bracketed-paste support across claude versions.** Claude Code's input handler is built on Ink (React). It should accept `\x1b[200~...\x1b[201~`, but the submit semantics on the closing CR have varied. If the Task 6 E2E test hangs at `send_turn`, try sending plain `prompt + "\r"` without bracketed-paste framing — the test's prompt is single-line so both forms work. Long-term we may need to detect the version and pick the right framing.
2. **First-prompt race.** `wait_for_jsonl` waits for the JSONL to appear, which empirically happens once claude has finished startup. If a future release defers JSONL creation to "after first user message," this strategy breaks. The fallback is a 500–1000 ms fixed delay after spawn plus a directory-watch to discover the filename.
3. **Permission prompts.** We pass `--permission-mode bypassPermissions` to avoid them entirely. If a future feature needs to show them (e.g. a "show me what claude would have done" mode), we'd need to detect them on the PTY screen — at which point the vt100 / alacritty_terminal route from the design discussion becomes relevant.
4. **Slash commands.** Not exposed in `send_turn` because they aren't "turns" — they're TUI control surfaces. If we ever want anatta to drive `/cost`, `/compact`, etc. for housekeeping, add a `send_slash_command(&self, cmd: &str)` method that writes `format!("/{cmd}\r")` without bracketed-paste wrapping.
5. **Cross-shape Claude→Claude swap.** Task 9's `swap` re-opens when shape differs (one-shot ↔ interactive). Correct, but it loses any warm-process advantage during the swap. Revisit if profile-swap mid-chat becomes common.
6. **`stderr_tail` is always empty** in interactive mode (PTY merges stdio). Downstream renderers should treat it as advisory.
7. **Worktree / SIGWINCH.** The PTY size is fixed at open. Since TUI output is discarded, that's benign — but if anatta ever attaches the TUI to its own UI, we'll need to forward window-size changes via `MasterPty::resize`.

---

## Self-Review

**Spec coverage:** Every component named in the architecture overview maps to a task — PTY spawn (T5), drain thread (T5), writer thread (T5), tail (T4 unit + T5 integration), session id via `--session-id` (T5), JSONL discovery (T2), prompt encoding (T3), `send_turn` (T6), `cancel` (T7), `close` (T8), `Session` / `BackendLaunch` / `TurnEvents` integration (T9), module docs (T10). The "risks & follow-ups" list is explicit about what we are NOT building (slash commands, screen scraping, SIGWINCH).

**Placeholder scan:** No "TBD"; no "add appropriate error handling"; no "similar to Task N" without code. Each step shows the actual code or the exact command + expected output.

**Type consistency:** `ClaudeInteractiveLaunch`, `ClaudeInteractiveSession`, `ClaudeInteractiveInterruptHandle`, `InteractiveTurnHandle`, `PtyCommand`, `ActiveTurn`, `encode_prompt`, `run_tail`, `persistent_tail_loop`, `push_synthetic_session_started`, `wait_for_jsonl`, `working_jsonl_path` — names are stable across all tasks. `Session::send_turn` returns `TurnEvents` (existing type, with new `ClaudeInteractive` variant); `ClaudeInteractiveSession::send_turn` returns `InteractiveTurnHandle`; Task 9 wraps the latter inside `TurnEventsInner::ClaudeInteractive { handle, interrupt }`, matching the codex shape. No naming drift.
