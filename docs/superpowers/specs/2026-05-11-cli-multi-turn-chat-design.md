# `anatta chat` — multi-turn CLI sessions with pretty rendering

**Status**: design accepted, ready to implement
**Date**: 2026-05-11
**Owner**: wxx

## Problem

Today `anatta send` is one-shot. There is no interactive multi-turn loop, and
the existing pretty-printer (`apps/anatta-cli/src/send.rs::render_pretty`) is
~50 lines of bracketed `eprintln!` lines with no styling. Two pain points:

1. **No multi-turn UX**: backend `--resume <id>` works at the spawn layer but
   the CLI exposes it only as a flag on a one-shot send. Users cannot hold a
   conversation, name it, or come back to it later.
2. **Plain-text rendering**: every `AgentEvent` variant is rendered as
   `[label] payload` lines. Markdown-flavored assistant replies, tool calls,
   thinking blocks all blur together.

This spec defines `anatta chat`: a named, persistent, locked, interactively
rendered multi-turn session.

## Goals

- One CLI subcommand family (`anatta chat {new,resume,ls,rm,unlock}`) that
  drives a multi-turn conversation against a single profile.
- Conversations persist in `anatta.db` so users can resume across CLI
  invocations.
- Per-conversation lock prevents two CLI processes from spawning the backend
  against the same on-disk session file concurrently.
- Renderer with markdown support for assistant text, distinct visual style
  for tool calls / thinking / errors / usage.
- Trait-based renderer so a future ratatui backend slots in without changing
  the chat loop.

## Non-Goals (v1)

- No transcript storage — we keep only the pointer (`backend_session_id`),
  not message bodies. Backend stores its own session JSONL.
- No `chat fork` / `chat rename` / `chat export` / `chat history` commands.
- No syntax highlighting for code blocks (no `syntect`).
- No full-screen TUI (no `ratatui`).
- No spinner / progress indication during tool execution.
- No transcript replay on `chat resume` — banner only.
- No chat-mode JSON output (use `anatta send --json --resume` for that).
- No multi-host lock coordination (single-machine assumption).
- No REPL meta commands beyond `/exit` and standard signals.
- **No cross-tool lock coordination**: `anatta send --resume <id>` does not
  consult the chat lock. A user running `send --resume X` while a `chat`
  session is live against the same backend_session_id can corrupt the
  underlying session file. Revisit if this bites; for v1, treat as user
  responsibility (the chat lock exists only between `anatta chat` instances).
- **Windows liveness fallback**: the PID liveness check uses `libc::kill(pid,
  0)` on Unix. On Windows v1, `pid_alive` returns `true` unconditionally so
  no automatic stale-lock reclaim happens — user runs `chat unlock`.
  Phase 2 daemon work will replace this with a portable mechanism.
- **Reboot PID-reuse**: across a host reboot, a leftover lock_holder_pid may
  alias an unrelated live process. We do not record process-start time to
  disambiguate; `chat unlock` is the escape hatch.

## Architecture

### Module dependency

```
apps/anatta-cli
  main.rs ──► chat/mod.rs                           ← typed error → exit code
                │
                ├─ Command::Chat {New,Resume,Ls,Rm,Unlock}
                │
                ├─ run_new/run_resume → chat/runner.rs
                │                          │
                │                          ├─ chat/lock.rs ──► libc::kill (cfg unix)
                │                          │      │
                │                          │      └─► store::conversation
                │                          │
                │                          ├─ chat/input.rs ─► rustyline
                │                          │
                │                          ├─ chat/render/mod (EventRenderer trait)
                │                          │      │
                │                          │      └─► line.rs ──► crossterm
                │                          │                │
                │                          │                ├─► markdown.rs ─► termimad
                │                          │                └─ uses Palette from mod.rs
                │                          │
                │                          ├─ send::build_claude_launch (refactor target)
                │                          ├─ send::build_codex_launch  (refactor target)
                │                          │
                │                          └─ runtime::spawn::AgentSession
                │                                  ↑ NEW: cancel_mut(&mut self)
                │
                ├─ ls/rm/unlock impls (inline in mod.rs) → store::conversation
                │
                └─ ChatError (in mod.rs)

crates/anatta-store
  lib.rs ──► profile.rs (existing, unchanged)
        └─► conversation.rs (new)

crates/anatta-runtime
  spawn/mod.rs (modified)                          ← add cancel_mut helper
```

### Crate boundary discipline

| Boundary | Enforcement |
|---|---|
| `anatta-store` knows nothing about process liveness | `try_acquire_with_check` accepts a `FnOnce(i64) -> bool` callback; CLI passes the libc-backed check. |
| `anatta-store` knows nothing about chat / rendering | New module `conversation.rs` is pure CRUD + lock SQL. |
| `chat::runner` knows nothing about rendering details | Holds `&mut dyn EventRenderer`. |
| `chat::render::line` knows nothing about termimad | All termimad calls go through `chat::render::markdown::render(&MadSkin, &str) -> String`. |
| `chat` does not duplicate `send`'s launch wiring | `send` exposes `pub(crate) build_claude_launch / build_codex_launch` helpers; chat calls them and reuses `SendError` for the launch-build path. |
| `anatta-runtime` changes are minimal and additive | Adds `AgentSession::cancel_mut(&mut self) -> Result<ExitInfo, SpawnError>` so the chat loop can `tokio::select!` between event drain and Ctrl-C without consuming the session. No existing API breaks. |

## Data model

### Schema

```sql
-- crates/anatta-store/migrations/0003_conversation.sql
CREATE TABLE conversations (
    name               TEXT PRIMARY KEY,
    profile_id         TEXT NOT NULL REFERENCES profiles(id) ON DELETE RESTRICT,
    backend_session_id TEXT,                    -- NULL until first turn completes
    cwd                TEXT NOT NULL,
    last_used_at       TEXT NOT NULL,           -- ISO-8601 UTC
    lock_holder_pid    INTEGER                  -- NULL = idle
);
```

6 columns. No indexes — row count is bounded to dozens.

### Field justification

| Field | Used by |
|---|---|
| `name` | All subcommands. PK because there is no rename in v1. |
| `profile_id` | Resume needs backend kind, auth method, provider env (all on `profiles`). FK with `ON DELETE RESTRICT` prevents orphaning. |
| `backend_session_id` | The pointer used in `--resume` / `exec resume`. NULL on insert; written exactly once after the first turn completes. |
| `cwd` | claude/codex sessions reference relative paths in tool outputs; resuming in a different cwd would be silently wrong. |
| `last_used_at` | `chat ls` ordering and humanized "5m ago" display. Updated on each turn. |
| `lock_holder_pid` | Mutual exclusion. NULL = idle. |

### Fields explicitly cut

- `id` (UUIDv7) — no rename, no child tables to FK from.
- `backend` ('claude'/'codex') — derive via JOIN on profiles.
- `created_at`, `turn_count` — display-only and unused.
- `lock_holder_hostname` — single-machine v1; add when daemon enters Phase 2.
- `lock_acquired_at` — staleness uses PID liveness, not timeout.
- All indexes — table cardinality is too small to matter.

## Multi-turn semantics

`claude --resume <id>` and `codex exec resume <id>` are **per-turn fresh
subprocesses**. Each turn = `spawn::launch` with the `resume` field set. The
backend reads its own on-disk session JSONL and continues. anatta keeps only:

1. `backend_session_id` in the row — pointer into backend's storage.
2. `lock_holder_pid` — exclusive ownership for the CLI process.

### Per-invocation flow

```
acquire lock (RAII guard, see Lock semantics)
    │
    ├─ load row (profile_id, backend_session_id, cwd)
    │
    ├─ render banner (chat name, profile, cwd, [session id if resume])
    │
    └─ loop:
        ├─ rustyline read prompt
        │     │
        │     └─ EOF (Ctrl-D) or empty line → break
        │
        ├─ build Launch {
        │       resume: row.backend_session_id.map(SessionId::new),
        │       prompt, profile, cwd, ...
        │   }
        │
        ├─ spawn::launch → AgentSession (first event already drained,
        │                                so session.session_id() is known)
        │
        ├─ tokio::select! {
        │       drain events into renderer,
        │       Ctrl-C → session.cancel_mut().await; break inner loop
        │   }
        │
        ├─ session.wait().await → ExitInfo            (after natural drain)
        │   OR cancel_mut path completed              (after Ctrl-C)
        │
        ├─ if first turn (backend_session_id was NULL):
        │       store.set_backend_session_id(name, sid)
        │   This commits the id even on cancelled/non-zero exit; the
        │   backend has already written SessionStarted to its on-disk
        │   session file, so the id is valid for future resume. A
        │   cancelled first turn just resumes from a near-empty file.
        │
        ├─ renderer.on_turn_end(&exit)
        ├─ store.touch_last_used(name)
        └─ continue
    │
    └─ guard.release_now() (explicit)  // Drop is fallback only
```

### Invariants

| Invariant | Guarantee |
|---|---|
| At most one CLI in `spawn::launch` per `name` at a time | Lock SQL atomic via `BEGIN IMMEDIATE`. |
| `backend_session_id` is monotonic (NULL → set → constant) | Only one write site, gated on `is_none()`. |
| `cwd` does not drift across turns | Read once at `chat resume`, passed to every `Launch`. |
| Subprocess crash does not corrupt next turn | Each turn is an independent process. Backend writes its own session file atomically. |
| Drop release failure is recoverable | Next acquire performs PID liveness check; `chat unlock` is the manual escape hatch. |

### First-turn failure policy

If `spawn::launch` fails on the first turn after the row is inserted, the
error propagates and the row is left with `backend_session_id = NULL`. The
lock releases via the RAII guard. On the next `chat resume`, attempting to
load a NULL session id will surface a clear error; the user runs
`chat rm <name>` and starts over. We do not write compensating cleanup logic
in v1.

## Lock semantics

### SQL contract

```rust
// crates/anatta-store/src/conversation.rs (sketch)
pub async fn try_acquire_with_check<F>(
    pool: &SqlitePool,
    name: &str,
    my_pid: i64,
    is_alive: F,
) -> Result<AcquireOutcome, StoreError>
where
    F: FnOnce(i64) -> bool,
{
    // Note: `pool.begin()` issues plain `BEGIN`, which on SQLite acquires
    // a SHARED lock that upgrades to RESERVED on first write. The two-step
    // SELECT-then-UPDATE inside this txn can race in theory, but SQLite
    // serializes writes globally so the worst case is a redundant retry.
    // For v1 this is acceptable. If needed, swap to a raw
    // `sqlx::query("BEGIN IMMEDIATE").execute(...)` to take RESERVED up front.
    let mut tx = pool.begin().await?;
    let row = sqlx::query!(
        "SELECT lock_holder_pid FROM conversations WHERE name = ?",
        name
    ).fetch_one(&mut *tx).await?;

    let can_take = match row.lock_holder_pid {
        None => true,
        Some(pid) => !is_alive(pid),
    };
    if !can_take {
        return Ok(AcquireOutcome::Held { pid: row.lock_holder_pid.unwrap() });
    }

    sqlx::query!(
        "UPDATE conversations SET lock_holder_pid = ? WHERE name = ?",
        my_pid, name
    ).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(AcquireOutcome::Acquired)
}
```

### `ConversationGuard` (RAII)

```rust
// apps/anatta-cli/src/chat/lock.rs (sketch)
pub struct ConversationGuard<'a> {
    store: &'a Store,
    name: String,
    holder_pid: i64,        // the PID we wrote; release SQL must match it
    released: bool,
}

impl<'a> ConversationGuard<'a> {
    pub async fn acquire(store: &'a Store, name: &str) -> Result<Self, ChatError> { ... }

    /// Explicit async release. Call this on the happy path before drop.
    /// SQL is keyed on (name, holder_pid) so we never clear a lock that a
    /// later acquirer (post force-unlock or post-stale-reclaim) now owns.
    pub async fn release_now(mut self) -> Result<(), ChatError> {
        store.release_lock_if_held(&self.name, self.holder_pid).await?;
        self.released = true;
        Ok(())
    }
}

impl<'a> Drop for ConversationGuard<'a> {
    fn drop(&mut self) {
        if self.released { return; }
        // best-effort detached release; main path uses release_now.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pool = self.store.pool().clone();
            let name = std::mem::take(&mut self.name);
            let pid = self.holder_pid;
            handle.spawn(async move {
                if let Err(e) = release_lock_if_held_query(&pool, &name, pid).await {
                    eprintln!("[anatta] warn: failed to release lock for '{name}': {e}");
                }
            });
        }
    }
}
```

`release_lock_if_held` SQL:

```sql
UPDATE conversations
SET lock_holder_pid = NULL
WHERE name = ?1 AND lock_holder_pid = ?2;
```

A delayed Drop after force-unlock + reacquire by another process becomes a
no-op (zero rows affected) instead of trampling the new holder.

### PID liveness

Provided by `libc::kill(pid, 0)` on Unix (`libc` is already a transitive
dependency via `tokio` / `sqlx`, so zero net new deps):

```rust
#[cfg(unix)]
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > libc::pid_t::MAX as i64 { return false; }
    // signal 0: do permission/existence check, do not deliver a signal.
    // returns 0 on success (process exists and we may signal it),
    // -1 on failure (errno = ESRCH no such process, EPERM exists but
    //  we lack permission to signal it — still alive from our POV).
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 { return true; }
    let errno = std::io::Error::last_os_error().raw_os_error();
    errno == Some(libc::EPERM)  // EPERM means "exists, just no perm"
}

#[cfg(not(unix))]
fn pid_alive(_pid: i64) -> bool {
    // Windows: be conservative — assume alive. User must `chat unlock`.
    true
}
```

One syscall per check, no process-table refresh, no extra crates. Trade-off:
Windows users lose automatic stale-lock reclaim and must run `chat unlock`
manually. Documented in non-goals.

### Failure modes

| Scenario | Outcome |
|---|---|
| Normal exit (`/exit`, Ctrl-D) | `release_now` runs synchronously; lock cleared. |
| Panic / `?` propagation | Drop fires, spawns detached task, prints warning if it fails. |
| SIGKILL / power loss | Lock is left set. Next acquire's PID liveness check reclaims it. |
| Stuck across reboot (PID reused) | Same — PID liveness sees no matching process. |
| Pathological PID-reuse collision | `chat unlock` is the manual escape hatch. |

## CLI surface

### Subcommands

```
anatta chat new <name> --profile <profile-id> [--cwd <path>]
    Insert row, acquire lock, enter loop. --cwd defaults to current dir.

anatta chat resume <name>
    Acquire lock, enter loop. cwd is read from row (no override).

anatta chat ls
    List conversations. Columns: NAME | PROFILE | LAST USED | STATUS.
    LAST USED is humanized ("5m ago", "yesterday").
    STATUS = "idle" or "🔒 pid <n>".

anatta chat rm <name>
    Delete row. Refuses if locked (require unlock or process exit first).
    Does not touch backend session files.

anatta chat unlock <name> [--yes]
    Force-clear lock fields. Interactive y/N prompt unless --yes.
```

### REPL meta

| Trigger | Effect |
|---|---|
| `/exit`, `/quit`, Ctrl-D | Release lock, exit 0. |
| Ctrl-C (turn running) | `session.cancel()`, render "(turn cancelled)", continue loop. |
| Ctrl-C (prompt waiting) | Same as `/exit`. |

No other meta commands. Users wanting `/clear`, `/model`, etc. exit and
restart with different args.

### Banners

`chat new`:

```
anatta chat · my-refactor
profile: claude-Ab12CdEf  ·  cwd: ~/code/repo  ·  model: <from SessionStarted>
─────────────────────────────────────────────────────────────────────────
> 
```

`chat resume`:

```
anatta chat · my-refactor (resumed)
profile: claude-Ab12CdEf  ·  cwd: ~/code/repo  ·  session: 8f3a…c2b1
─────────────────────────────────────────────────────────────────────────
> 
```

Model name and the rest of `SessionStarted` fields print after the banner
because they arrive in the first event.

### `unlock` confirmation

```
$ anatta chat unlock my-refactor
warning: forcibly clearing lock for 'my-refactor' (was held by pid 41023).
         if another anatta chat is still running, the underlying session
         file may be corrupted by concurrent writes. proceed? [y/N]
```

`--yes` skips the prompt.

## Rendering design

### `EventRenderer` trait

```rust
// apps/anatta-cli/src/chat/render/mod.rs
pub(crate) trait EventRenderer {
    fn on_session_started(&mut self, model: &str, cwd: &str);
    fn on_event(&mut self, ev: &AgentEvent);
    fn on_turn_end(&mut self, exit: &ExitInfo);
    fn on_chat_end(&mut self);
}
```

CLI-internal trait. No public API surface; signature changes are free.

### Delta + final emission model

Both projectors emit BOTH delta and final variants today (verified in
`crates/anatta-runtime/src/claude/projector.rs:248,373` and
`crates/anatta-runtime/src/codex/projector.rs:303,328`). Snapshot semantics:
each `*Delta` carries the FULL accumulated text in `text_so_far`. The
final `AssistantText` / `Thinking` carries the same text once the block
closes.

Codex always uses `content_block_index = 0` (single AgentMessage per item);
Claude uses real per-block indices (a turn can have many text blocks
interleaved with thinking and tool blocks).

Policy:

- **TTY mode**: paint each delta in place (cursor anchor + clear + redraw);
  the final variant for the same logical block is then a no-op (text already
  on screen). Implementation: when a final arrives, locate the open
  region for its block and mark closed; do not repaint.
- **Non-TTY mode**: skip all delta variants entirely; paint only the final.

This avoids the "match final to delta by index" ambiguity (final lacks an
index): in TTY mode we never need to match because the screen is already
correct. In non-TTY mode we don't paint deltas, so finals are the sole
source.

### `LineRenderer` per-payload behavior

| Payload | TTY treatment | Non-TTY treatment |
|---|---|---|
| `SessionStarted` | No-op (banner already printed). | Same. |
| `TurnStarted`, `UserPrompt` | No-op. | Same. |
| `AssistantTextDelta { idx, text_so_far }` | If no anchor for `idx`: capture cursor row, paint `markdown::render(text_so_far)`. Else: cursor-move to anchor → clear-down → repaint markdown. Update `last_lines` from painted height. | Skip. |
| `AssistantText { text }` | Find open anchor (lowest unclosed block_index); if found, mark closed (text already painted). If none open, paint markdown at cursor. | Paint markdown at cursor. |
| `ThinkingDelta { idx, text_so_far }` | Same anchor mechanics, dim grey, `│ ` per line, no markdown. | Skip. |
| `Thinking { text }` | Same close-only logic as AssistantText. If no anchor open, paint dim grey block. | Paint dim grey block. |
| `ToolUse { name, input, id }` | Cyan one-line: `⚙ Read(file_path="…", limit=200)`. First 2 object fields, truncate values to 60 chars, " …+N fields" if more. If input not object, `to_string()` truncated to 100. Record `(id → anchor_row, name, finalized=false)` in tool_anchors. | Same, no anchor recording. |
| `ToolUseInputDelta` | Skip (matches the "minimal" decision; codex never emits it). | Skip. |
| `ToolResult { tool_use_id, success, text, structured }` | Locate anchor for `tool_use_id`. **Summary** = first present of: text head (60 chars), structured one-line JSON head (60 chars), or "(no output)". Backfill on anchor row: ` ✓ <summary>` (green) or ` ✗ <summary>` (red). If text > 200 chars, render dim 4-line preview below + "…(M lines more)". If anchor missing/scrolled-away, print a new line `↩ <name> ✓/✗ <summary>`. Mark anchor `finalized = true`. | Same minus cursor moves: print full one-line tool/result on a single line, no backfill. |
| `Usage` | Dim: `· 1.2k in · 480 out · $0.0034`. Cost omitted if None. | Same minus dim color. |
| `TurnCompleted` | For each tool_anchor with `finalized=false`, append ` …` (faint, no result indicator — codex WebSearch/TodoList never emit ToolResult; this is normal). Then render terminal-width separator + blank line. Reset all anchors. | Same minus separator coloring. |
| `RateLimit` | Yellow `⚠ rate limit ({kind}) — resets at <iso>`. | Same minus color. |
| `Error { fatal }` | Red `✗ error: <msg>`. Fatal adds line "session terminated". | Same minus color. |

### Anchor data structures

```rust
struct LineRenderer {
    is_tty: bool,
    text_blocks: BTreeMap<u32, TextAnchor>,       // open assistant text blocks
    thinking_blocks: BTreeMap<u32, TextAnchor>,   // open thinking blocks
    tool_anchors: HashMap<String, ToolAnchor>,    // by tool_use_id
    skin: termimad::MadSkin,
}

struct TextAnchor {
    start_row: u16,
    last_lines: u16,
    closed: bool,    // set when matching final arrives; kept until TurnCompleted resets
}

struct ToolAnchor {
    row: u16,
    name: String,
    finalized: bool,
}
```

### Codex `index = 0` collision handling

Codex never opens more than one AssistantTextDelta block per turn (single
AgentMessage item). With one open block at index 0, the BTreeMap holds at
most one entry — no collision in practice. Claude can have multiple text
blocks at distinct indices, also fine. The renderer does not need to
distinguish backends.

If a future projector ever emits two open codex blocks with index=0 (it
doesn't today), the second one would clobber the first's anchor; this is
out of scope for v1 and would need projector cooperation to fix.

### Markdown rendering (`markdown.rs`)

```rust
// apps/anatta-cli/src/chat/render/markdown.rs
use termimad::MadSkin;
use crossterm::style::Color;
use super::PALETTE;

pub(super) fn build_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::White);
    skin.inline_code.set_fg(PALETTE.tool);
    skin.code_block.set_bg(Color::AnsiValue(236));
    skin.headers.iter_mut().for_each(|h| h.set_fg(Color::White));
    skin.bullet.set_fg(PALETTE.thinking);
    skin.quote_mark.set_fg(PALETTE.thinking);
    skin.horizontal_rule.set_fg(PALETTE.separator);
    skin
}

pub(super) fn render(skin: &MadSkin, text: &str) -> String {
    skin.text(text, None).to_string()
}
```

- `AssistantText` is the only payload that goes through markdown.
- Tables, code fences, lists, headers, bold/italic/inline-code all covered
  by termimad.
- No syntax highlighting for code blocks (intentional — would pull syntect).
- Streaming: each delta re-renders full `text_so_far`; termimad is stateless
  so this is correct.

### Palette

```rust
// apps/anatta-cli/src/chat/render/mod.rs (alongside trait)
pub(super) struct Palette {
    pub assistant: Color,
    pub thinking: Color,
    pub tool: Color,
    pub tool_ok: Color,
    pub tool_err: Color,
    pub error: Color,
    pub rate_limit: Color,
    pub usage: Color,
    pub separator: Color,
    pub banner_dim: Color,
}

pub(super) const PALETTE: Palette = Palette {
    assistant: Color::Reset,
    thinking: Color::DarkGrey,
    tool: Color::Cyan,
    tool_ok: Color::Green,
    tool_err: Color::Red,
    error: Color::Red,
    rate_limit: Color::Yellow,
    usage: Color::DarkGrey,
    separator: Color::DarkGrey,
    banner_dim: Color::DarkGrey,
};
```

No theme switching. No config file. Edit source to change.

### Non-tty degradation

`crossterm`'s `IsTerminal` (or `std::io::IsTerminal`) detects pipe/redirect.
On non-tty:

- All ANSI escape sequences omitted.
- Cursor moves omitted (no anchor map populated).
- Delta variants entirely skipped.
- termimad output is bypassed entirely on non-tty; we print the original
  markdown source string verbatim instead. Tool input/output already render
  as plain text on tty too, so non-tty just removes color, not structure.

Effectively: pipe `anatta chat resume foo > out.txt` produces a clean text
log of the conversation.

## Error handling

```rust
// apps/anatta-cli/src/chat/mod.rs
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("conversation '{0}' not found")]
    NotFound(String),
    #[error("conversation '{0}' already exists")]
    AlreadyExists(String),
    #[error("conversation '{name}' is in use by pid {pid}")]
    Locked { name: String, pid: i64 },
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error("input ended (Ctrl-D)")]
    InputClosed,                          // Ok-path sentinel; main exits 0

    #[error(transparent)] Send(#[from] crate::send::SendError),
    #[error(transparent)] Store(#[from] anatta_store::StoreError),
    #[error(transparent)] Spawn(#[from] anatta_runtime::spawn::SpawnError),
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error("readline: {0}")] Readline(String),
}
```

### Exit code mapping

| Variant | Exit code |
|---|---|
| `InputClosed` | 0 (no error printed) |
| `Locked` | 2 (with `hint: anatta chat unlock {name}`) |
| `NotFound`, `AlreadyExists`, `ProfileNotFound` | 2 |
| All others | 1 |

`main.rs` is updated to recognize `InputClosed` and silence the `anatta:`
prefix for it.

## Testing strategy

### `crates/anatta-store/src/conversation.rs` unit tests

- `insert + get_by_name` round-trip
- `set_backend_session_id` NULL → set
- `try_acquire_with_check` basic acquire/release/re-acquire
- `try_acquire_with_check` with `is_alive=true` returns `Held`
- `try_acquire_with_check` with `is_alive=false` reclaims
- `rm` rejects locked rows
- `force_unlock` clears lock_holder_pid

### `apps/anatta-cli/src/chat/render/line.rs` snapshot tests

Use `insta` against a `Vec<u8>` writer instead of stdout.

- `AssistantText` markdown roundtrip (verify ANSI bold/italic present)
- Non-tty mode strips ANSI
- `ToolUse` formatting (object input, scalar input, truncation)
- `ToolResult` backfill onto preceding ToolUse line
- `ToolResult` fallback when anchor missing
- All payload variants render without panic

### `apps/anatta-cli/tests/chat_e2e.rs`

`crates/anatta-runtime/tests/spawn_mock.rs` lives in another crate's
integration test tree and is not importable. The CLI test file duplicates
the minimal mock-binary harness (~30 lines: write a small shell/rust script
that prints NDJSON for SessionStarted + AssistantText + TurnCompleted,
chmod +x, point `binary_path` at it). Keep it inline; do not promote
spawn_mock to a `mock` feature for v1.

- First turn writes `backend_session_id` to row
- Second turn passes `--resume <id>` argument
- Two CLI processes against same name: second gets `Locked`
- Kill the first, second succeeds (PID liveness)

### Manual smoke test (in spec)

```
[ ] anatta chat new foo --profile claude-XXX → prompt appears
[ ] type "hello" → assistant reply renders with markdown styling
[ ] Ctrl-C mid-turn → cancels turn, returns to prompt
[ ] /exit → lock released, row preserved
[ ] anatta chat resume foo (same shell) → "(resumed)" banner, continues
[ ] anatta chat resume foo (other shell, while first running) → Locked error
[ ] kill -9 first shell → second resume succeeds
[ ] anatta chat ls → row visible
[ ] anatta chat rm foo → deleted
[ ] anatta chat resume foo > out.txt → no ANSI in file
```

## File inventory

### New files (10)

| Path | Approx LOC | Responsibility |
|---|---|---|
| `crates/anatta-store/migrations/0003_conversation.sql` | 15 | DDL for `conversations` table. |
| `crates/anatta-store/src/conversation.rs` | 200 | `Conversation` struct, CRUD, lock SQL with callback-based liveness. |
| `apps/anatta-cli/src/chat/mod.rs` | 120 | `ChatCommand`, `run()`, `ChatError`, inline impls of `ls/rm/unlock`. |
| `apps/anatta-cli/src/chat/runner.rs` | 150 | `run_new`/`run_resume`; main loop body shared via private helper. |
| `apps/anatta-cli/src/chat/lock.rs` | 80 | `ConversationGuard` (RAII), `pid_alive` (sysinfo). |
| `apps/anatta-cli/src/chat/input.rs` | 60 | rustyline wrapper, history file path, signal semantics. |
| `apps/anatta-cli/src/chat/render/mod.rs` | 60 | `EventRenderer` trait, `Palette` + `PALETTE` const, `LineRenderer::new()` factory. |
| `apps/anatta-cli/src/chat/render/line.rs` | 280 | `LineRenderer`: anchor maps, dispatch, cursor ops, tty detection. |
| `apps/anatta-cli/src/chat/render/markdown.rs` | 50 | `build_skin`, `render` — termimad encapsulation. |
| `apps/anatta-cli/tests/chat_e2e.rs` | 150 | 4 integration tests (see Testing). |

### Modified files (5)

| Path | Change |
|---|---|
| `crates/anatta-store/src/lib.rs` | `pub mod conversation;` |
| `crates/anatta-runtime/src/spawn/mod.rs` | Add `pub async fn cancel_mut(&mut self) -> Result<ExitInfo, SpawnError>`. Refactor existing `cancel(self)` to delegate to it (or keep as-is and have both). Surface change is purely additive; no existing callers need to change. |
| `apps/anatta-cli/src/send.rs` | Extract `pub(crate) async fn build_claude_launch(record, prompt, resume, cwd, cfg) -> Result<ClaudeLaunch, SendError>` and `pub(crate) async fn build_codex_launch(...)` from inside `run_claude` / `run_codex`. The existing `run_*` functions become thin wrappers that call the builder + `spawn::launch` + `stream_session`. |
| `apps/anatta-cli/src/main.rs` | `mod chat;` + `Command::Chat` variant + dispatch arm. **Refactor error handling**: replace `result.map_err(|e| e.to_string())` with a typed dispatch (e.g. `match` on each command's error type) so `chat::ChatError::InputClosed` can map to `exit(0)` without printing the `anatta:` prefix. |
| `apps/anatta-cli/Cargo.toml` | Add `crossterm`, `termimad`, `rustyline`, `libc`; dev-dep `insta`. |

### New dependencies

| Crate | Purpose | Approx transitive count |
|---|---|---|
| `crossterm` | Cursor, style, IsTerminal | ~8 |
| `termimad` | Markdown → ANSI | ~8 (minimad, lazy-regex, unicode-width, coolor, ...) |
| `rustyline` | Line editing + history | ~10 |
| `libc` | `kill(pid, 0)` PID liveness on Unix; already a transitive dep, made direct | 0 net new |
| `insta` (dev) | Snapshot tests | ~12 |

`anatta-runtime` gets a small additive API change (`cancel_mut`). No other
crates touched. `anatta-core`, `anatta-worktree`, `anatta-server-core`,
`anatta-daemon-core`, `anatta-server`, `anatta-daemon` are unchanged.

## Out of scope (revisit later)

- Daemon-mediated chat (Phase 2) — daemon orchestrates spawn, CLI is a
  thin remote client. The `chat` subsystem here will likely be reused in
  spirit but not literally.
- ratatui full-screen mode — the `EventRenderer` trait is the seam for
  this.
- Transcript persistence — would require new tables and storage policy.
  Defer until users actually ask for "show me yesterday's chat".
- Cross-host lock — `lock_holder_hostname` migration when daemon enters.
- Code block syntax highlighting — would pull `syntect`. Defer.
- `chat fork`, `chat rename`, `chat send` (one-shot resume) — none compelling
  for v1.

## Open questions

None blocking implementation. Known residuals:

- termimad quirks (unclosed code fences during delta streaming, OSC 8
  hyperlink behavior on legacy terminals) — both have documented fallback
  strategies above.
- SELECT-then-UPDATE in `try_acquire_with_check` has no retry path; a
  failed SQLite serialization surfaces as `StoreError::Sqlx` and the user
  re-runs `chat resume`. Acceptable for v1 single-user usage.
- Cancelled first turn leaves the backend session file with only
  `SessionStarted` (and possibly partial UserPrompt) events. Resume from
  this state works (claude/codex accept short histories) but produces a
  visually empty first turn on replay. Documented behavior, not a bug.
