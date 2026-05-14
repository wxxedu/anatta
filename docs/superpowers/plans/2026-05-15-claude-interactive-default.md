# Claude Interactive as Default Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the interactive PTY session shape the default for Claude across the anatta CLI; add a `--per-turn` opt-out flag; fix the runtime so opening a session no longer hangs waiting for claude to write its session JSONL (claude 2.1.x defers JSONL creation until first user input).

**Architecture:** Three layered changes. (1) **Runtime fix** — drop `wait_for_jsonl` from `ClaudeInteractiveSession::open()` and replace it with a fixed 500ms startup sleep; the tail loop already polls until the file exists, so no functional gap. Delete the now-unused `wait_for_jsonl` helper + its tests (YAGNI). (2) **CLI launch builder** — add `build_claude_interactive` mirroring `build_claude`; derive `bare: bool` from `record.auth_method` (false for OAuth/Login, true for ApiKey). (3) **Dispatcher + CLI args** — `build_launch` gains a `per_turn: bool`; the new `--per-turn` flag on `anatta send` and on `anatta chat new` / `anatta chat resume` plumbs through to it.

**Tech Stack:** Rust workspace; `tokio`; existing `portable-pty`; `clap` 4.x for CLI args; `anatta-store::ProfileRecord` (unchanged — no migration).

---

## File Structure

**Modified (5 files):**

- `crates/anatta-runtime/src/spawn/claude_interactive.rs` — remove `wait_for_jsonl` call from `open()` and remove the `STARTUP_TIMEOUT` constant + kill-on-timeout branch; add `tokio::time::sleep(STARTUP_SLEEP)` (500ms) for first-prompt safety; remove the now-unused `use crate::conversation::paths::wait_for_jsonl;` import.
- `crates/anatta-runtime/src/conversation/paths.rs` — delete the `wait_for_jsonl` function and its three tests (no remaining caller). The `use std::time::Duration;` import becomes unused; delete it too.
- `apps/anatta-cli/src/launch.rs` — add `build_claude_interactive(...)` alongside `build_claude(...)`; update `build_launch` signature to take `per_turn: bool`; add `LaunchError::PerTurnIgnoredForCodex` (or treat silently — plan picks silent below).
- `apps/anatta-cli/src/send.rs` — add `#[arg(long)] per_turn: bool` to `SendArgs`; pass to `build_launch`.
- `apps/anatta-cli/src/chat/mod.rs` + `apps/anatta-cli/src/chat/runner.rs` — add `--per-turn` to `ChatCommand::New` and `ChatCommand::Resume`; thread `per_turn: bool` through `run_new` / `run_resume` / `drive_chat` and pass to both `build_launch` call sites (initial + cross-engine swap path).
- `crates/anatta-runtime/tests/spawn_e2e.rs` — drop `bare: true` from the two interactive E2E tests (the launch struct field stays, but tests now use `bare: false` matching what `build_claude_interactive` will derive for OAuth).

Single responsibility audit: `claude_interactive.rs` is still focused on PTY+JSONL transport. `launch.rs` is still focused on profile→launch resolution. `send.rs` / `chat/*.rs` still own CLI command behavior. No split needed.

---

## Task 1: Remove `wait_for_jsonl` gate from `open()` and add startup sleep

**Why:** The current `open()` blocks for up to 15s waiting for claude to write its session JSONL. Empirically, claude 2.1.141 doesn't create the JSONL until the first user message arrives — so `open()` always times out. We don't need this gate at all: the tail task already handles "file doesn't exist yet, sleep and retry." The only real concern is that the PTY isn't ready to receive bracketed-paste input the instant we return from `open()`; a small fixed sleep covers that.

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`

- [ ] **Step 1: Read the current `open()` body**

Read `crates/anatta-runtime/src/spawn/claude_interactive.rs`. Locate:
- `const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);` near the other constants.
- The block inside `open()` that calls `wait_for_jsonl(&jsonl, STARTUP_TIMEOUT).await` and the `if let Err(e) = ... { let _ = child.kill(); return Err(...); }` cleanup.
- The `let cwd_str = ...; let jsonl = working_jsonl_path(...)` block immediately above it.
- The import `use crate::conversation::paths::{wait_for_jsonl, working_jsonl_path};`.

- [ ] **Step 2: Add `STARTUP_SLEEP` constant**

In the constants block (near `STARTUP_TIMEOUT` / `CLOSE_GRACE`), add:

```rust
/// After spawn, sleep this long before returning from `open()` so the
/// PTY input handler has time to be ready for bracketed-paste keystrokes
/// from the first `send_turn`. Empirically ~200 ms is enough for claude
/// 2.1.x on macOS; 500 ms is a defensive default.
const STARTUP_SLEEP: Duration = Duration::from_millis(500);
```

Remove `const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);` — no longer used.

- [ ] **Step 3: Replace the `wait_for_jsonl` block with the sleep**

In the body of `open()`, find this block:

```rust
let cwd_str = launch
    .cwd
    .to_str()
    .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;
let jsonl = working_jsonl_path(&launch.profile.path, cwd_str, session_id.as_str());
if let Err(e) = wait_for_jsonl(&jsonl, STARTUP_TIMEOUT).await {
    let _ = child.kill();
    return Err(SpawnError::Io(std::io::Error::other(format!(
        "claude did not write its session JSONL at {} within {:?}: {}",
        jsonl.display(),
        STARTUP_TIMEOUT,
        e,
    ))));
}
```

Replace with:

```rust
let cwd_str = launch
    .cwd
    .to_str()
    .ok_or_else(|| SpawnError::Io(std::io::Error::other("cwd is not UTF-8")))?;
let jsonl = working_jsonl_path(&launch.profile.path, cwd_str, session_id.as_str());

// Give claude's PTY input handler a moment to be ready for our first
// bracketed-paste prompt. Claude defers writing the session JSONL until
// the first user message, so we cannot use file presence as a readiness
// signal — the tail task handles "file doesn't exist yet" via retry.
tokio::time::sleep(STARTUP_SLEEP).await;
```

- [ ] **Step 4: Update the import**

Change:

```rust
use crate::conversation::paths::{wait_for_jsonl, working_jsonl_path};
```

to:

```rust
use crate::conversation::paths::working_jsonl_path;
```

- [ ] **Step 5: Build clean**

Run: `cargo build -p anatta-runtime --features spawn`

Expected: no errors, no new warnings.

- [ ] **Step 6: Existing tests still pass**

Run: `cargo test -p anatta-runtime --features spawn`

Expected: same count as before this task (~230 passing, 5 ignored). The `wait_for_jsonl` tests still pass because the helper still exists — Task 2 removes them.

- [ ] **Step 7: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs
git commit -m "fix(runtime): drop wait_for_jsonl gate in interactive open() — claude defers JSONL until first prompt"
```

---

## Task 2: Delete the unused `wait_for_jsonl` helper

**Why:** With Task 1 done, nothing in the runtime calls `wait_for_jsonl`. Dead code; remove it and its tests. The helper was added speculatively for the original plan's startup gate; that approach turned out wrong, and YAGNI says don't keep tested-but-unused utilities around.

**Files:**
- Modify: `crates/anatta-runtime/src/conversation/paths.rs`

- [ ] **Step 1: Verify no remaining callers**

Run: `grep -rn "wait_for_jsonl" crates/anatta-runtime/ apps/`

Expected: only matches in `crates/anatta-runtime/src/conversation/paths.rs` itself (the fn + its 3 tests). If there are other callers, STOP and report — you missed something in Task 1.

- [ ] **Step 2: Delete the function**

In `crates/anatta-runtime/src/conversation/paths.rs`, locate and delete:

```rust
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
```

- [ ] **Step 3: Delete the three tests**

In the `#[cfg(test)] mod tests { ... }` block of the same file, locate and delete:

```rust
    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_returns_immediately_when_present() { /* … */ }

    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_returns_when_file_appears() { /* … */ }

    #[cfg(feature = "spawn")]
    #[tokio::test]
    async fn wait_for_jsonl_times_out() { /* … */ }
```

(Three contiguous `#[cfg(feature = "spawn")] #[tokio::test]`-decorated test fns.)

- [ ] **Step 4: Remove now-unused imports**

At the top of `crates/anatta-runtime/src/conversation/paths.rs`, the `use std::time::Duration;` is now unused (it was only referenced by `wait_for_jsonl`). Delete that line.

If the top imports were:

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;
```

After this step:

```rust
use std::path::{Path, PathBuf};
```

- [ ] **Step 5: Build clean, tests pass**

Run: `cargo build -p anatta-runtime --features spawn`

Expected: clean.

Run: `cargo test -p anatta-runtime --features spawn --lib paths`

Expected: 6 tests pass (down from 9 — the 3 wait_for_jsonl tests are gone, the 6 encode_cwd/path-layout tests remain).

Run: `cargo test -p anatta-runtime --features spawn`

Expected: full suite passes (~3 fewer tests than before — same delta).

- [ ] **Step 6: Commit**

```bash
git add crates/anatta-runtime/src/conversation/paths.rs
git commit -m "chore(runtime): remove unused wait_for_jsonl helper + tests"
```

---

## Task 3: Add `build_claude_interactive` to CLI launch builder

**Why:** Today `apps/anatta-cli/src/launch.rs` only produces `BackendLaunch::Claude(...)`. To dispatch to the new shape, we need a parallel `build_claude_interactive(...)` that returns a `ClaudeInteractiveLaunch`. The function is mostly a copy-paste of `build_claude`, with three differences: (1) returns `ClaudeInteractiveLaunch`, (2) no `prompt` field, (3) `bare` derived from `record.auth_method` (OAuth/Login profiles need keychain access, so `bare: false`; ApiKey profiles can use `bare: true` for predictability).

**Files:**
- Modify: `apps/anatta-cli/src/launch.rs`

- [ ] **Step 1: Read the current `build_claude` to understand the pattern**

Read `apps/anatta-cli/src/launch.rs` lines ~73-111 (the `build_claude` function). Note the:
- `ClaudeProfileId::from_string` + `ClaudeProfile::open` path.
- Binary location via `auth::locate_binary("claude")`.
- API key + provider env handling (the `match (record.auth_method, api_key)` block).
- The struct fields populated at the end.

- [ ] **Step 2: Update the import block**

At the top of `apps/anatta-cli/src/launch.rs`, change:

```rust
use anatta_runtime::spawn::{
    BackendLaunch, ClaudeLaunch, ClaudeSessionId, CodexLaunch, CodexThreadId,
};
```

to:

```rust
use anatta_runtime::spawn::{
    BackendLaunch, ClaudeInteractiveLaunch, ClaudeLaunch, ClaudeSessionId, CodexLaunch,
    CodexThreadId,
};
```

- [ ] **Step 3: Add `build_claude_interactive` function**

Append the new function immediately after `build_claude` (before `build_codex`):

```rust
fn build_claude_interactive(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<ClaudeInteractiveLaunch, LaunchError> {
    let id = ClaudeProfileId::from_string(record.id.clone())?;
    let profile = ClaudeProfile::open(id, &cfg.anatta_home)?;
    let binary_path = auth::locate_binary("claude").ok_or(LaunchError::BinaryNotFound("claude"))?;

    let api_key = read_api_key_for(record, cfg)?;
    let provider = match (record.auth_method, api_key) {
        (AuthMethod::Login, _) => None,
        (AuthMethod::ApiKey, None) => return Err(LaunchError::ApiKeyMissing(record.id.clone())),
        (AuthMethod::ApiKey, Some(token)) => {
            let spec = providers::lookup(&record.provider)
                .ok_or_else(|| LaunchError::UnknownProvider(record.provider.clone()))?;
            let overrides = Overrides {
                base_url: record.base_url_override.clone(),
                model: record.model_override.clone(),
                small_fast_model: record.small_fast_model_override.clone(),
                default_opus_model: record.default_opus_model_override.clone(),
                default_sonnet_model: record.default_sonnet_model_override.clone(),
                default_haiku_model: record.default_haiku_model_override.clone(),
                subagent_model: record.subagent_model_override.clone(),
            };
            Some(ProviderEnv::build(spec, &overrides, token))
        }
    };

    // `--bare` is incompatible with OAuth/keychain auth (it explicitly
    // disables keychain reads). Use it only for ApiKey profiles, where
    // it gives a clean predictable environment (no hooks, no LSP, no
    // plugin sync, no CLAUDE.md auto-discovery).
    let bare = matches!(record.auth_method, AuthMethod::ApiKey);

    Ok(ClaudeInteractiveLaunch {
        profile,
        cwd,
        resume: resume.map(ClaudeSessionId::new),
        binary_path,
        provider,
        model: record.model_override.clone(),
        bare,
    })
}
```

- [ ] **Step 4: Build clean**

Run: `cargo build -p anatta-cli`

Expected: clean. The new function is dead-code-warning-free because Task 4 will call it.

- [ ] **Step 5: Commit**

```bash
git add apps/anatta-cli/src/launch.rs
git commit -m "feat(cli): build_claude_interactive constructs ClaudeInteractiveLaunch from profile"
```

---

## Task 4: Wire `per_turn` parameter through `build_launch` dispatcher

**Why:** Now that `build_claude_interactive` exists, the dispatcher needs to pick between it and `build_claude`. The choice is per-call (`per_turn: bool` argument). For Codex, the flag is silently ignored (codex has only the persistent-app-server shape).

**Files:**
- Modify: `apps/anatta-cli/src/launch.rs`

- [ ] **Step 1: Update `build_launch` signature**

Replace the current `build_launch`:

```rust
pub fn build_launch(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    cfg: &Config,
) -> Result<BackendLaunch, LaunchError> {
    match record.backend {
        BackendKind::Claude => build_claude(record, cwd, resume, cfg).map(BackendLaunch::Claude),
        BackendKind::Codex => build_codex(record, cwd, resume, cfg).map(BackendLaunch::Codex),
    }
}
```

with:

```rust
pub fn build_launch(
    record: &ProfileRecord,
    cwd: PathBuf,
    resume: Option<String>,
    per_turn: bool,
    cfg: &Config,
) -> Result<BackendLaunch, LaunchError> {
    match (record.backend, per_turn) {
        (BackendKind::Claude, false) => build_claude_interactive(record, cwd, resume, cfg)
            .map(BackendLaunch::ClaudeInteractive),
        (BackendKind::Claude, true) => build_claude(record, cwd, resume, cfg)
            .map(BackendLaunch::Claude),
        // Codex has only one session shape; the per_turn flag is a no-op
        // for codex profiles.
        (BackendKind::Codex, _) => build_codex(record, cwd, resume, cfg).map(BackendLaunch::Codex),
    }
}
```

- [ ] **Step 2: Build will now fail at call sites**

Run: `cargo build -p anatta-cli`

Expected: two compile errors at the call sites — `apps/anatta-cli/src/send.rs:124` and `apps/anatta-cli/src/chat/runner.rs:162` and `:277`. These are expected; Tasks 5 and 6 fix them.

- [ ] **Step 3: Commit**

```bash
git add apps/anatta-cli/src/launch.rs
git commit -m "feat(cli): build_launch dispatches Claude+!per_turn → ClaudeInteractive"
```

The compile error stays until Tasks 5 + 6 land. That's the intended TDD-of-callsites sequence; don't try to fix the call sites in this task.

---

## Task 5: Add `--per-turn` flag to `anatta send`

**Why:** Surface the per-invocation choice on the `send` command. Default (no flag) → interactive. `--per-turn` → traditional `--print` one-shot.

**Files:**
- Modify: `apps/anatta-cli/src/send.rs`

- [ ] **Step 1: Read the current `SendArgs` struct**

Read `apps/anatta-cli/src/send.rs` around line 18-35 to confirm the field order pattern (resume, etc.). Note where `build_launch` is called (line 124).

- [ ] **Step 2: Add the `per_turn` field**

In `apps/anatta-cli/src/send.rs`, find the `SendArgs` struct (currently has `prompt`, profile selectors, `resume`, etc.). Add a new field at the end of the struct (before the closing brace):

```rust
    /// Force per-turn (`--print` stream-json) Claude session shape
    /// instead of the new default (interactive PTY). Ignored for Codex
    /// profiles. Useful for environments where keychain access fails
    /// from the subprocess.
    #[arg(long)]
    per_turn: bool,
```

- [ ] **Step 3: Pass the flag to `build_launch`**

Find this line (currently `send.rs:124`):

```rust
let launch = launch::build_launch(&record, cwd, args.resume, cfg)?;
```

Change to:

```rust
let launch = launch::build_launch(&record, cwd, args.resume, args.per_turn, cfg)?;
```

- [ ] **Step 4: Build clean**

Run: `cargo build -p anatta-cli`

Expected: send.rs is now clean. The remaining compile errors are in `chat/runner.rs` (fixed by Task 6).

- [ ] **Step 5: Commit**

```bash
git add apps/anatta-cli/src/send.rs
git commit -m "feat(cli): anatta send --per-turn flag (default: interactive)"
```

---

## Task 6: Add `--per-turn` flag to `anatta chat` + thread through runner

**Why:** Same surface as Task 5, but for `chat`. Two CLI subcommands (`new` and `resume`) need the flag; one runner state field carries it through cross-engine swap.

**Files:**
- Modify: `apps/anatta-cli/src/chat/mod.rs`
- Modify: `apps/anatta-cli/src/chat/runner.rs`

- [ ] **Step 1: Add `per_turn` to both `ChatCommand` variants**

In `apps/anatta-cli/src/chat/mod.rs`, find the `ChatCommand` enum (lines ~38-63). Modify `New` and `Resume`:

```rust
#[derive(Debug, Subcommand)]
pub enum ChatCommand {
    /// Start a new named conversation against a profile.
    New {
        /// Conversation name (must be unique).
        name: String,
        /// Profile id (e.g., `claude-Ab12CdEf`).
        #[arg(long, short = 'p')]
        profile: String,
        /// Working directory the backend runs in (default: cwd).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Force per-turn Claude session shape instead of the default
        /// interactive PTY. No-op for Codex profiles.
        #[arg(long)]
        per_turn: bool,
    },
    /// Resume an existing conversation.
    Resume {
        /// Conversation name.
        name: String,
        /// Force per-turn Claude session shape instead of the default
        /// interactive PTY. No-op for Codex profiles.
        #[arg(long)]
        per_turn: bool,
    },
    /// List conversations.
    Ls,
    /// Delete a conversation (refuses if in use).
    Rm {
        /// Conversation name.
        name: String,
    },
}
```

- [ ] **Step 2: Locate the dispatch site in `chat/mod.rs`**

Search `apps/anatta-cli/src/chat/mod.rs` for where `ChatCommand::New { .. }` is matched and `runner::run_new(...)` is called. The variant pattern needs to bind `per_turn` and pass it through:

```rust
ChatCommand::New { name, profile, cwd, per_turn } => {
    runner::run_new(name, profile, cwd, per_turn, cfg).await
}
ChatCommand::Resume { name, per_turn } => {
    runner::run_resume(name, per_turn, cfg).await
}
```

(Exact existing call shape may differ slightly — preserve the existing argument order and append `per_turn` as a new positional or named arg. If `run_new` already takes `cfg` as the trailing arg, insert `per_turn` before it.)

- [ ] **Step 3: Update `run_new` / `run_resume` in `runner.rs`**

In `apps/anatta-cli/src/chat/runner.rs`, find the `pub(crate) async fn run_new(...)` and `pub(crate) async fn run_resume(...)` signatures. Add `per_turn: bool` parameter to each. Both should pass it down to `drive_chat`.

For `run_new` (current signature around line ~30 of runner.rs):

```rust
pub(crate) async fn run_new(
    name: String,
    profile_id: String,
    cwd: Option<PathBuf>,
    per_turn: bool,
    cfg: &Config,
) -> Result<(), ChatError> {
    // … existing body …
    drive_chat(conv, profile, cfg, /* resumed = */ false, per_turn).await
}
```

For `run_resume` (current signature around line ~84):

```rust
pub(crate) async fn run_resume(
    name: String,
    per_turn: bool,
    cfg: &Config,
) -> Result<(), ChatError> {
    // … existing body …
    drive_chat(conv, profile, cfg, /* resumed = */ true, per_turn).await
}
```

- [ ] **Step 4: Update `drive_chat` signature and threading**

Find `drive_chat` (currently around line 100 of `runner.rs`). Add `per_turn: bool` to the signature:

```rust
async fn drive_chat(
    conv: ConversationRecord,
    profile: ProfileRecord,
    cfg: &Config,
    resumed: bool,
    per_turn: bool,
) -> Result<(), ChatError> {
    // … existing body …
```

- [ ] **Step 5: Pass `per_turn` to the two `build_launch` call sites**

In `drive_chat` body, find both `launch::build_launch(...)` calls (at current line ~162 and ~277). Add `per_turn` as the new arg:

```rust
let launch = launch::build_launch(
    &profile,
    conv.cwd.clone().into(),
    backend_session_id.clone(),
    per_turn,                       // ← new arg
    cfg,
)?;
```

And similarly for the swap-path call at line ~277:

```rust
let new_launch = match launch::build_launch(
    &new_profile,
    conv.cwd.clone().into(),
    resume_id.clone(),
    per_turn,                       // ← new arg
    cfg,
) {
    Ok(l) => l,
    Err(e) => {
        eprintln!("✗ build_launch failed: {e}");
        // … existing handling …
    }
};
```

(Exact arg order: match `build_launch`'s signature from Task 4: `(record, cwd, resume, per_turn, cfg)`.)

- [ ] **Step 6: Build clean**

Run: `cargo build -p anatta-cli`

Expected: clean — all compile errors from Task 4 are resolved.

- [ ] **Step 7: Run full test suite**

Run: `cargo test --workspace --features spawn`

Expected: every non-ignored test passes. No new failures.

- [ ] **Step 8: Commit**

```bash
git add apps/anatta-cli/src/chat/mod.rs apps/anatta-cli/src/chat/runner.rs
git commit -m "feat(cli): anatta chat --per-turn flag (default: interactive)"
```

---

## Task 7: Drop `bare: true` override from interactive E2E tests

**Why:** The previous E2E test fix (`90e1f3d`) explicitly set `bare: false` in the test invocations because the global default `bare: true` was wrong for OAuth users. Now that `build_claude_interactive` derives `bare` from `auth_method`, the test should let the launch struct's natural defaults apply. The E2E tests construct `ClaudeInteractiveLaunch` directly (not via `build_claude_interactive`), so they still need to explicitly say `bare: false` — but the comment justifying it now points at `build_claude_interactive` as the source of truth.

**Files:**
- Modify: `crates/anatta-runtime/tests/spawn_e2e.rs`

- [ ] **Step 1: Update the two interactive E2E tests**

In `crates/anatta-runtime/tests/spawn_e2e.rs`, find the two test functions: `launch_real_claude_interactive_emits_session_started_assistant_completion` and `interactive_cancel_closes_turn_channel`. Both currently have `bare: false` (the prior fix). The struct-construction line(s) look like:

```rust
let launch = ClaudeInteractiveLaunch {
    profile,
    cwd,
    resume: None,
    binary_path: bin,
    provider: None,
    model: None,
    bare: false,
};
```

Add a comment above the `bare: false` line explaining the choice (because the test bypasses `build_claude_interactive`, it must replicate the same derivation):

```rust
    // OAuth-based ~/.claude profiles require keychain access, which
    // `--bare` disables. The CLI's `build_claude_interactive` derives
    // `bare` from `record.auth_method`; this test constructs the launch
    // directly, so it must hard-code the OAuth-compatible value.
    bare: false,
```

- [ ] **Step 2: Run the E2E tests in --no-run mode**

Run: `cargo test -p anatta-runtime --features spawn --test spawn_e2e --no-run`

Expected: clean compile.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --workspace --features spawn`

Expected: all non-ignored tests pass.

- [ ] **Step 4: Document the manual E2E procedure**

The two `#[ignore]`-d interactive E2E tests require a non-Claude-Code shell environment to access the macOS keychain. Add a note to the top of `crates/anatta-runtime/tests/spawn_e2e.rs` (right after the `//!` block):

If the file already has a `//!` doc block, append a new paragraph at the end:

```
//!
//! NOTE for the interactive PTY tests
//! (`launch_real_claude_interactive_emits_session_started_assistant_completion`
//! and `interactive_cancel_closes_turn_channel`): macOS keychain access
//! is gated per-process, so these will fail with "Not logged in" if run
//! from inside a Claude Code session subprocess. Run from a regular
//! terminal:
//!
//! ```bash
//! cargo test -p anatta-runtime --features spawn --test spawn_e2e -- \
//!     --ignored --nocapture launch_real_claude_interactive interactive_cancel
//! ```
```

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "docs(runtime): explain bare: false + manual E2E run in interactive tests"
```

---

## Risks & follow-ups (deferred; not part of this plan)

1. **First-prompt input loss under heavy startup latency.** The 500 ms `STARTUP_SLEEP` is a heuristic. On slower machines or under cold-disk conditions, claude's TUI input handler might not be ready in 500 ms; the bracketed-paste bytes would land in a pre-startup PTY buffer. The robust fix is to monitor PTY output for any byte before sending input (i.e., "claude has started writing"). Out of scope here — flip if reliability issues surface.

2. **Per-turn-on-Codex is a silent no-op.** The plan keeps it silent. If we later want a warning, it's a one-line addition to `build_launch`. The current dispatcher already routes Codex correctly regardless of the flag.

3. **No CI coverage for the new dispatcher.** A pure-unit test exercising `build_launch` on a fake `ProfileRecord` with `per_turn=true`/`false` and both backends would lock in the dispatch behavior. Not added here because the launch builder pulls in `ClaudeProfile::open` which touches the real filesystem; the workaround is a unit test in `launch.rs` that constructs a mock profile dir via `tempfile`. Marked as future work.

4. **Cross-engine swap to a different `per_turn` is impossible today.** `drive_chat` stores `per_turn` once at the top and reuses it for every `build_launch`. If a future feature wants "swap to a different profile WITH a different session-shape," it'd need a slash command (`/per-turn` / `/interactive`) that updates the runner state. Out of scope.

5. **`anatta send` for codex profiles ignores `--per-turn` silently.** Same as item 2.

---

## Self-Review

**Spec coverage:**

- Drop `wait_for_jsonl` from `open()` → Task 1.
- Delete the unused helper + tests → Task 2.
- Add `build_claude_interactive` → Task 3.
- Update `build_launch` dispatcher to take `per_turn` → Task 4.
- `--per-turn` on `anatta send` → Task 5.
- `--per-turn` on `anatta chat new` and `anatta chat resume` → Task 6.
- Update E2E tests + document manual run → Task 7.

All goals from the conversation are covered.

**Placeholder scan:**

- No "TBD" / "implement later" placeholders.
- No "add error handling" without code.
- Each code step shows the actual edit.
- One place I lean on context — Task 6 step 2/3 says "exact existing call shape may differ slightly — preserve the existing argument order." That's flagged because `runner.rs` evolves and I don't want to lock in a brittle 5-arg ordering. The implementer must read the file. Acceptable tradeoff: better than committing a wrong arg order that compiles but does the wrong thing.

**Type consistency:**

- `per_turn: bool` is the parameter name everywhere (struct field, function arg, CLI flag).
- `--per-turn` is the kebab-case CLI form (clap converts `per_turn` → `per-turn` automatically; no explicit `#[arg(long = "per-turn")]` needed).
- `build_claude_interactive` is the function name in both Task 3 (definition) and Task 4 (call site).
- `STARTUP_SLEEP` is consistently the new constant name (replacing `STARTUP_TIMEOUT`).
- Task 7's E2E test names match what's in `spawn_e2e.rs` today (verified via the conversation's earlier grep output).

No drift detected.
