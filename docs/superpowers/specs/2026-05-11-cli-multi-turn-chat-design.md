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

## Architecture

### Module dependency

```
apps/anatta-cli
  main.rs ──► chat/mod.rs
                │
                ├─ Command::Chat {New,Resume,Ls,Rm,Unlock}
                │
                ├─ run_new/run_resume → chat/runner.rs
                │                          │
                │                          ├─ chat/lock.rs ──► sysinfo
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
                │                          └─ runtime::spawn  (existing, unchanged)
                │
                ├─ ls/rm/unlock impls (inline in mod.rs) → store::conversation
                │
                └─ ChatError (in mod.rs)

crates/anatta-store
  lib.rs ──► profile.rs (existing, unchanged)
        └─► conversation.rs (new)
```

### Crate boundary discipline

| Boundary | Enforcement |
|---|---|
| `anatta-store` knows nothing about `sysinfo` | `try_acquire_with_check` accepts a `FnOnce(i64) -> bool` callback for liveness; CLI passes the sysinfo-backed check. |
| `anatta-store` knows nothing about chat / rendering | New module `conversation.rs` is pure CRUD + lock SQL. |
| `chat::runner` knows nothing about rendering details | Holds `&mut dyn EventRenderer`. |
| `chat::render::line` knows nothing about termimad | All termimad calls go through `chat::render::markdown::render(&MadSkin, &str) -> String`. |
| `anatta-runtime` is unchanged | The existing `spawn::launch` + `AgentSession` API is sufficient. |

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
        ├─ spawn::launch → AgentSession
        │
        ├─ if first turn (backend_session_id was NULL):
        │       store.set_backend_session_id(name, session.session_id())
        │
        ├─ tokio::select! {
        │       drain events into renderer,
        │       Ctrl-C → session.cancel(), continue loop,
        │   }
        │
        ├─ session.wait() → ExitInfo
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
    released: bool,
}

impl<'a> ConversationGuard<'a> {
    pub async fn acquire(store: &'a Store, name: &str) -> Result<Self, ChatError> { ... }

    /// Explicit async release. Call this on the happy path before drop.
    pub async fn release_now(mut self) -> Result<(), ChatError> {
        store.release_lock(&self.name).await?;
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
            handle.spawn(async move {
                if let Err(e) = release_lock_query(&pool, &name).await {
                    eprintln!("[anatta] warn: failed to release lock for '{name}': {e}");
                }
            });
        }
    }
}
```

### PID liveness

Provided by `sysinfo`:

```rust
fn pid_alive(pid: i64) -> bool {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes();
    sys.process(sysinfo::Pid::from(pid as usize)).is_some()
}
```

`sysinfo` is cross-platform, ~6-7 transitive deps. Lighter than `nix` for the
amount we need. CLI-only dep; the store crate stays free of process
introspection.

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

### `LineRenderer` per-payload behavior

| Payload | Treatment |
|---|---|
| `SessionStarted` | No-op. Banner already printed. |
| `TurnStarted`, `UserPrompt` | No-op. |
| `AssistantText` | `markdown::render(&skin, text)` → write. Final variants do not carry `content_block_index`. Policy: maintain `last_open_text_block: Option<u32>`; if set, clear from its anchor row first, then unset. Otherwise append at cursor. |
| `AssistantTextDelta` | Forward-looking (projector v1 doesn't emit). On tty: clear from anchor → re-render full `text_so_far` via markdown; set `last_open_text_block = Some(content_block_index)`. Non-tty: skip. |
| `Thinking` | Dim grey, prefix `│` per line, no markdown. Default fully expanded. |
| `ThinkingDelta` | Same anchoring as AssistantTextDelta, dim grey. |
| `ToolUse { name, input, id }` | Cyan one-line: `⚙ Read(file_path="…", limit=200)`. Fields: take first 2 from object, value-truncate to 60 chars, " …+N fields" if more. Record (id → cursor row, name) in tool_anchors. |
| `ToolUseInputDelta` | Skip in v1. |
| `ToolResult { tool_use_id, success, text }` | Backfill onto the ToolUse anchor row: ` ✓ 47 lines` (green) or ` ✗ exit 101` (red). If text > 200 chars, render dim 4-line preview below + "…(M lines more)". If anchor scrolled away, fallback to a new line `↩ <name> ✓/✗`. |
| `Usage` | Dim: `· 1.2k in · 480 out · $0.0034`. Cost omitted if None. |
| `TurnCompleted` | Render terminal-width separator + blank line. |
| `RateLimit` | Yellow `⚠ rate limit ({kind}) — resets at <iso>`. |
| `Error { fatal }` | Red `✗ error: <msg>`. Fatal adds line "session terminated". |

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

Reuses the existing mock-backend infrastructure (`spawn_mock`).

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

### Modified files (3)

| Path | Change |
|---|---|
| `crates/anatta-store/src/lib.rs` | `pub mod conversation;` |
| `apps/anatta-cli/src/main.rs` | `mod chat;` + `Command::Chat` variant + dispatch arm. |
| `apps/anatta-cli/Cargo.toml` | Add `crossterm`, `termimad`, `rustyline`, `sysinfo`; dev-dep `insta`. |

### New dependencies

| Crate | Purpose | Approx transitive count |
|---|---|---|
| `crossterm` | Cursor, style, IsTerminal | ~8 |
| `termimad` | Markdown → ANSI | ~8 (minimad, lazy-regex, unicode-width, coolor, ...) |
| `rustyline` | Line editing + history | ~10 |
| `sysinfo` | PID liveness | ~6 |
| `insta` (dev) | Snapshot tests | ~12 |

No changes to `anatta-runtime`, `anatta-core`, `anatta-worktree`,
`anatta-server-core`, `anatta-daemon-core`, or any binary other than
`anatta-cli`.

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

None remaining at design time. Implementation may surface termimad quirks
(unclosed code fences during streaming, OSC 8 hyperlink behavior on legacy
terminals); both have documented fallback strategies above.
