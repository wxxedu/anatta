# Permission-Level Cycling (Shift+Tab) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user press Shift+Tab in the anatta chat REPL to cycle through a unified permission-level enum that maps coherently to both claude (interactive PTY + per-turn) and codex backends, including claude's `auto` mode (AI-judged per-call risk) and its codex analogue (`approvals_reviewer = auto_review`).

**Architecture:** Define a `PermissionLevel` enum in `anatta-core` shared by both backends. Each backend exposes `set_permission_level(level)`. Claude (PTY) writes `\x1b[Z` to cycle in-band; claude (per-turn) updates its launch template for the next spawn; codex updates the per-turn `(approval_policy, sandbox)` in `turn/start` and, on transitions across the Auto boundary, closes-and-reopens the app-server with the right `-c approvals_reviewer=...` flag and resumes via `thread/resume` (per [#22](https://github.com/wxxedu/anatta/issues/22)). The chat REPL binds Shift+Tab via rustyline's `ConditionalEventHandler` and dispatches to `Session::set_permission_level`.

**Tech Stack:** Rust workspace; `tokio`; `portable-pty`; `rustyline` 17 (with `ConditionalEventHandler`); existing claude / codex CLI binaries.

---

## File Structure

**Created:**
- `crates/anatta-core/src/permission.rs` — `PermissionLevel` enum + cycle/label helpers + per-backend mapping methods.
- `apps/anatta-cli/src/chat/permission_hotkey.rs` — rustyline `ConditionalEventHandler` that turns Shift+Tab into a `CyclePermission` read outcome.

**Modified:**
- `crates/anatta-core/src/lib.rs` — `pub mod permission; pub use permission::PermissionLevel;`.
- `crates/anatta-runtime/src/spawn/session.rs` — store current level on each session variant; add `Session::permission_level()` + `Session::set_permission_level()`.
- `crates/anatta-runtime/src/spawn/claude.rs` — `ClaudeLaunch.permission_level: PermissionLevel`; build `--permission-mode` from it.
- `crates/anatta-runtime/src/spawn/claude_interactive.rs` — track `current_level`; on `set_permission_level(new)`, write the right count of `\x1b[Z`; pass initial `--permission-mode` from level.
- `crates/anatta-runtime/src/spawn/codex/mod.rs` — drop `APPROVAL_POLICY` / `SANDBOX_POLICY` constants; add `CodexPolicy { approval, sandbox, reviewer }` derived from a `PermissionLevel`.
- `crates/anatta-runtime/src/spawn/codex/handshake.rs` — accept `(approval, sandbox, reviewer)` as args instead of using constants; pass `-c approvals_reviewer=...` to the CLI when armed.
- `crates/anatta-runtime/src/spawn/codex/launch.rs` — same; one-shot path.
- `crates/anatta-runtime/src/spawn/codex/persistent.rs` — store `current_level`; rewrite `turn/start` to use level-derived policy; implement `set_permission_level(new)` with close-and-reopen on Auto transition.
- `crates/anatta-runtime/src/profile/family.rs` (or wherever per-profile defaults live) — default `PermissionLevel` per backend.
- `apps/anatta-cli/src/chat/input.rs` — wire the custom event handler; add `ReadOutcome::CyclePermission`.
- `apps/anatta-cli/src/chat/mod.rs` — re-export the hotkey module.
- `apps/anatta-cli/src/chat/runner.rs` — handle `ReadOutcome::CyclePermission` by calling `session.set_permission_level(current.next())`; re-render the status line.
- `apps/anatta-cli/src/chat/render.rs` (or wherever the per-prompt banner is printed) — render current level above `> `.
- `apps/anatta-cli/src/launch.rs` — plumb `PermissionLevel` (initial default = `Default`) into both `build_claude_interactive` and `build_codex`.
- `crates/anatta-runtime/src/spawn/claude_interactive.rs` (extend `ensure_onboarding_complete`) — pre-seed `autoPermissionsNotificationCount` so claude's auto-mode opt-in dialog never fires.

Decomposition rationale: the enum + mappings sit in `anatta-core` so the CLI and both backend modules depend only on the core. Each backend impl is in its own file so cross-cutting changes stay local. The REPL hotkey is in its own file (`permission_hotkey.rs`) because it's a single, isolated rustyline abstraction.

---

## Task 1: Define `PermissionLevel` enum (TDD)

**Files:**
- Create: `crates/anatta-core/src/permission.rs`
- Modify: `crates/anatta-core/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/anatta-core/src/permission.rs`:

```rust
//! User-facing permission level for backend sessions.
//!
//! This is the unified abstraction over claude's `--permission-mode`
//! and codex's `(approval_policy, sandbox, approvals_reviewer)` axes.
//! The chat REPL cycles through it via Shift+Tab; each backend maps
//! the level to its own native shape.

use serde::{Deserialize, Serialize};

/// Trust level for tool calls. Ordered from most-restrictive to most-permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionLevel {
    /// Read-only: backend may not write or execute.
    Plan,
    /// Ask per tool call (default).
    Default,
    /// Auto-accept file edits; ask for the rest.
    AcceptEdits,
    /// AI-judged: the model evaluates each tool call for risk.
    Auto,
    /// Skip all permission checks (sandbox is `danger-full-access`).
    BypassAll,
}

impl PermissionLevel {
    /// Cycle order used by the Shift+Tab keybinding.
    pub const CYCLE: [PermissionLevel; 5] = [
        PermissionLevel::Default,
        PermissionLevel::AcceptEdits,
        PermissionLevel::Auto,
        PermissionLevel::BypassAll,
        PermissionLevel::Plan,
    ];

    /// Next level in the cycle. Wraps around.
    pub fn next(self) -> Self {
        let idx = Self::CYCLE.iter().position(|&l| l == self).unwrap_or(0);
        Self::CYCLE[(idx + 1) % Self::CYCLE.len()]
    }

    /// Short human-readable label used in the REPL status line.
    pub fn label(self) -> &'static str {
        match self {
            PermissionLevel::Plan => "plan",
            PermissionLevel::Default => "default",
            PermissionLevel::AcceptEdits => "accept edits",
            PermissionLevel::Auto => "auto",
            PermissionLevel::BypassAll => "bypass all",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cycles_through_all_levels_in_order() {
        let mut cur = PermissionLevel::Default;
        let order: Vec<_> = (0..5).map(|_| { let next = cur.next(); cur = next; next }).collect();
        assert_eq!(
            order,
            vec![
                PermissionLevel::AcceptEdits,
                PermissionLevel::Auto,
                PermissionLevel::BypassAll,
                PermissionLevel::Plan,
                PermissionLevel::Default,
            ]
        );
    }

    #[test]
    fn next_wraps_around_after_plan() {
        assert_eq!(PermissionLevel::Plan.next(), PermissionLevel::Default);
    }

    #[test]
    fn label_is_short_and_lowercase() {
        for l in PermissionLevel::CYCLE {
            let label = l.label();
            assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == ' '));
            assert!(label.len() <= 16);
        }
    }
}
```

In `crates/anatta-core/src/lib.rs`, add the module + re-export. Find the existing `pub mod` lines and add:

```rust
pub mod permission;
pub use permission::PermissionLevel;
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p anatta-core permission`

Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/anatta-core/src/permission.rs crates/anatta-core/src/lib.rs
git commit -m "feat(core): PermissionLevel enum + cycle/label helpers"
```

---

## Task 2: Claude mapping (`PermissionLevel` → `--permission-mode` value)

**Files:**
- Modify: `crates/anatta-core/src/permission.rs`

- [ ] **Step 1: Write the failing tests**

Append to the existing `#[cfg(test)] mod tests` block in `crates/anatta-core/src/permission.rs`:

```rust
    #[test]
    fn claude_arg_matches_known_mode_names() {
        assert_eq!(PermissionLevel::Default.claude_arg(), "default");
        assert_eq!(PermissionLevel::AcceptEdits.claude_arg(), "acceptEdits");
        assert_eq!(PermissionLevel::Auto.claude_arg(), "auto");
        assert_eq!(PermissionLevel::BypassAll.claude_arg(), "bypassPermissions");
        assert_eq!(PermissionLevel::Plan.claude_arg(), "plan");
    }
```

- [ ] **Step 2: Run the new test to confirm it fails**

Run: `cargo test -p anatta-core permission::tests::claude_arg`

Expected: compile error — `claude_arg` does not exist.

- [ ] **Step 3: Implement `claude_arg`**

Append to the `impl PermissionLevel` block in `crates/anatta-core/src/permission.rs`:

```rust
    /// Value to pass as `claude --permission-mode <value>`. The string
    /// must be one of claude's documented choices: `default | acceptEdits
    /// | auto | bypassPermissions | plan | dontAsk` (we don't expose
    /// `dontAsk` — see plan rationale).
    pub fn claude_arg(self) -> &'static str {
        match self {
            PermissionLevel::Default => "default",
            PermissionLevel::AcceptEdits => "acceptEdits",
            PermissionLevel::Auto => "auto",
            PermissionLevel::BypassAll => "bypassPermissions",
            PermissionLevel::Plan => "plan",
        }
    }
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p anatta-core permission`

Expected: 4 tests pass (3 prior + 1 new).

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-core/src/permission.rs
git commit -m "feat(core): PermissionLevel::claude_arg mapping"
```

---

## Task 3: Codex mapping (`PermissionLevel` → `CodexPolicy`)

**Files:**
- Modify: `crates/anatta-core/src/permission.rs`

- [ ] **Step 1: Write the failing tests**

Append to the same test block:

```rust
    #[test]
    fn codex_policy_matches_design_table() {
        let p = PermissionLevel::Default.codex_policy();
        assert_eq!(p.approval, "on-request");
        assert_eq!(p.sandbox, "workspace-write");
        assert_eq!(p.reviewer_armed, false);

        let p = PermissionLevel::AcceptEdits.codex_policy();
        assert_eq!(p.approval, "never");
        assert_eq!(p.sandbox, "workspace-write");
        assert_eq!(p.reviewer_armed, false);

        let p = PermissionLevel::Auto.codex_policy();
        assert_eq!(p.approval, "on-request");
        assert_eq!(p.sandbox, "workspace-write");
        assert_eq!(p.reviewer_armed, true);

        let p = PermissionLevel::BypassAll.codex_policy();
        assert_eq!(p.approval, "never");
        assert_eq!(p.sandbox, "danger-full-access");
        assert_eq!(p.reviewer_armed, false);

        let p = PermissionLevel::Plan.codex_policy();
        assert_eq!(p.approval, "on-request");
        assert_eq!(p.sandbox, "read-only");
        assert_eq!(p.reviewer_armed, false);
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p anatta-core permission::tests::codex_policy`

Expected: compile error — `CodexPolicy` / `codex_policy` not defined.

- [ ] **Step 3: Implement `CodexPolicy` and `codex_policy()`**

Append to `crates/anatta-core/src/permission.rs` (after the existing `impl` block):

```rust
/// Codex-side policy resolved from a [`PermissionLevel`]. The first
/// two fields are passed per-turn in the `turn/start` JSON-RPC body;
/// `reviewer_armed` requires session-level configuration (`-c
/// approvals_reviewer=auto_review` at codex CLI startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexPolicy {
    pub approval: &'static str,
    pub sandbox: &'static str,
    pub reviewer_armed: bool,
}

impl PermissionLevel {
    pub fn codex_policy(self) -> CodexPolicy {
        match self {
            PermissionLevel::Default => CodexPolicy {
                approval: "on-request",
                sandbox: "workspace-write",
                reviewer_armed: false,
            },
            PermissionLevel::AcceptEdits => CodexPolicy {
                approval: "never",
                sandbox: "workspace-write",
                reviewer_armed: false,
            },
            PermissionLevel::Auto => CodexPolicy {
                approval: "on-request",
                sandbox: "workspace-write",
                reviewer_armed: true,
            },
            PermissionLevel::BypassAll => CodexPolicy {
                approval: "never",
                sandbox: "danger-full-access",
                reviewer_armed: false,
            },
            PermissionLevel::Plan => CodexPolicy {
                approval: "on-request",
                sandbox: "read-only",
                reviewer_armed: false,
            },
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p anatta-core permission`

Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-core/src/permission.rs
git commit -m "feat(core): PermissionLevel::codex_policy + CodexPolicy struct"
```

---

## Task 4: Plumb `PermissionLevel` into `ClaudeLaunch` (per-turn)

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude.rs`

- [ ] **Step 1: Read the current `ClaudeLaunch` shape**

Read `crates/anatta-runtime/src/spawn/claude.rs` to confirm the existing struct shape (it should have `profile`, `cwd`, `prompt`, `resume`, `binary_path`, `provider`).

- [ ] **Step 2: Add `permission_level` field**

In `crates/anatta-runtime/src/spawn/claude.rs`, locate the `pub struct ClaudeLaunch { ... }` block. Add a new field at the end (before the closing `}`):

```rust
    /// Initial permission level. Mapped to `--permission-mode <value>`
    /// at spawn. The per-turn shape re-spawns claude on every turn, so
    /// updating this between turns takes effect on the next turn.
    pub permission_level: anatta_core::PermissionLevel,
```

- [ ] **Step 3: Update the argv assembly**

Find the existing `cmd.arg("--dangerously-skip-permissions");` line (around line 61). Replace it with:

```rust
        cmd.arg("--permission-mode").arg(self.permission_level.claude_arg());
```

- [ ] **Step 4: Update existing callers**

Build will fail: `cargo build -p anatta-runtime --features spawn`. The compile errors point at every place that constructs `ClaudeLaunch`. Fix each by adding `permission_level: anatta_core::PermissionLevel::Default` to the struct literal. Likely call sites:

- `apps/anatta-cli/src/launch.rs::build_claude` — add the field.
- `crates/anatta-runtime/tests/spawn_e2e.rs::launch_real_claude_emits_*` — add the field.

- [ ] **Step 5: Build + run all non-ignored tests**

Run: `cargo build -p anatta-runtime --features spawn` — clean.
Run: `cargo test -p anatta-runtime --features spawn` — all pre-existing tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude.rs apps/anatta-cli/src/launch.rs crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "feat(runtime): ClaudeLaunch.permission_level → --permission-mode argv"
```

---

## Task 5: Plumb `PermissionLevel` into `ClaudeInteractiveLaunch` + track current level

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`

- [ ] **Step 1: Add `permission_level` field to `ClaudeInteractiveLaunch`**

In `crates/anatta-runtime/src/spawn/claude_interactive.rs`, find `pub struct ClaudeInteractiveLaunch { ... }`. Add at the end:

```rust
    /// Initial permission level. Mapped to `--permission-mode <value>`
    /// at spawn; the session tracks subsequent transitions via
    /// `set_permission_level`.
    pub permission_level: anatta_core::PermissionLevel,
```

- [ ] **Step 2: Replace hardcoded `--permission-mode dontAsk` argv**

In `open()`, find the block:

```rust
        cmd.arg("--permission-mode");
        cmd.arg("dontAsk");
```

(The exact location is near the other `cmd.arg(...)` calls.) Replace with:

```rust
        cmd.arg("--permission-mode").arg(launch.permission_level.claude_arg());
```

Remove the surrounding doc-block that explains why `dontAsk` was chosen — it's no longer accurate.

- [ ] **Step 3: Track the current level on the session**

In the `pub struct ClaudeInteractiveSession { ... }` block, add a field:

```rust
    /// Last level we instructed claude to be at. Used to compute the
    /// number of `\x1b[Z` (Shift+Tab) writes needed to reach a new
    /// target without scraping the TUI's status bar.
    current_level: std::sync::Mutex<anatta_core::PermissionLevel>,
```

Wrap in `std::sync::Mutex` because `set_permission_level` mutates it via a `&self` reference (the session is shared across REPL + drain/writer threads).

- [ ] **Step 4: Initialize `current_level` in `open()`**

In `open()`, just before the final `Ok(Self { ... })`, capture the launch level:

```rust
        let initial_level = launch.permission_level;
```

And in the struct literal, add:

```rust
            current_level: std::sync::Mutex::new(initial_level),
```

- [ ] **Step 5: Build**

Run: `cargo build -p anatta-runtime --features spawn`

Expected: compile errors at callers that construct `ClaudeInteractiveLaunch`. Fix:

- `apps/anatta-cli/src/launch.rs::build_claude_interactive` — add `permission_level: anatta_core::PermissionLevel::Default`.
- `crates/anatta-runtime/tests/spawn_e2e.rs` — add the same to both `launch_real_claude_interactive_*` test bodies.

After fixes, `cargo build -p anatta-cli` should be clean.

- [ ] **Step 6: Run tests**

Run: `cargo test -p anatta-runtime --features spawn` — pre-existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs \
        apps/anatta-cli/src/launch.rs \
        crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "feat(runtime): ClaudeInteractiveLaunch.permission_level + current_level tracker"
```

---

## Task 6: `ClaudeInteractiveSession::set_permission_level` (TDD via unit test on cycle arithmetic)

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`

- [ ] **Step 1: Write the failing test for the cycle-distance helper**

In `crates/anatta-runtime/src/spawn/claude_interactive.rs`, locate the `#[cfg(test)] mod tests { ... }` block at the bottom of the file (or create one if absent — append before the file's closing). Add:

```rust
#[cfg(test)]
mod tests_perm {
    use super::shift_tab_count;
    use anatta_core::PermissionLevel;

    #[test]
    fn shift_tab_count_zero_when_same() {
        assert_eq!(shift_tab_count(PermissionLevel::Default, PermissionLevel::Default), 0);
    }

    #[test]
    fn shift_tab_count_steps_forward_in_cycle() {
        // CYCLE = [Default, AcceptEdits, Auto, BypassAll, Plan]
        assert_eq!(shift_tab_count(PermissionLevel::Default, PermissionLevel::AcceptEdits), 1);
        assert_eq!(shift_tab_count(PermissionLevel::Default, PermissionLevel::Auto), 2);
        assert_eq!(shift_tab_count(PermissionLevel::Default, PermissionLevel::BypassAll), 3);
        assert_eq!(shift_tab_count(PermissionLevel::Default, PermissionLevel::Plan), 4);
    }

    #[test]
    fn shift_tab_count_wraps_backwards_via_forward_steps() {
        // Plan → Default is 1 forward step (wraps).
        assert_eq!(shift_tab_count(PermissionLevel::Plan, PermissionLevel::Default), 1);
        // BypassAll → AcceptEdits = forward through Plan, Default, AcceptEdits = 3.
        assert_eq!(shift_tab_count(PermissionLevel::BypassAll, PermissionLevel::AcceptEdits), 3);
    }
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p anatta-runtime --features spawn --lib tests_perm`

Expected: compile error — `shift_tab_count` doesn't exist.

- [ ] **Step 3: Implement `shift_tab_count` + `set_permission_level`**

In `crates/anatta-runtime/src/spawn/claude_interactive.rs`, near the prompt-encoding section (around `encode_prompt`), add a new helper:

```rust
/// Number of Shift+Tab keystrokes required to advance claude's
/// internal permission-mode cursor from `from` to `to`, given that
/// each Shift+Tab moves one slot forward in `PermissionLevel::CYCLE`.
pub(crate) fn shift_tab_count(
    from: anatta_core::PermissionLevel,
    to: anatta_core::PermissionLevel,
) -> usize {
    let cycle = anatta_core::PermissionLevel::CYCLE;
    let f = cycle.iter().position(|&l| l == from).unwrap_or(0);
    let t = cycle.iter().position(|&l| l == to).unwrap_or(0);
    (t + cycle.len() - f) % cycle.len()
}
```

Then add the `set_permission_level` method on `ClaudeInteractiveSession`. Append to the `impl ClaudeInteractiveSession { ... }` block:

```rust
    /// Cycle claude's TUI permission mode by writing `\x1b[Z` (Shift+Tab)
    /// `N` times via the PTY writer, where `N` is the forward distance
    /// from the current level to `target` in `PermissionLevel::CYCLE`.
    /// Updates the local tracker so subsequent calls compute correctly.
    pub async fn set_permission_level(
        &self,
        target: anatta_core::PermissionLevel,
    ) -> Result<(), SpawnError> {
        let n = {
            let mut guard = self
                .current_level
                .lock()
                .expect("permission_level mutex poisoned");
            let n = shift_tab_count(*guard, target);
            *guard = target;
            n
        };
        if n == 0 {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(n * 3);
        for _ in 0..n {
            bytes.extend_from_slice(b"\x1b[Z");
        }
        self.pty_tx
            .send(PtyCommand::Write(bytes))
            .await
            .map_err(|_| SpawnError::Io(std::io::Error::other("pty writer task gone")))
    }

    /// Current tracked permission level.
    pub fn permission_level(&self) -> anatta_core::PermissionLevel {
        *self
            .current_level
            .lock()
            .expect("permission_level mutex poisoned")
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p anatta-runtime --features spawn --lib tests_perm`

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs
git commit -m "feat(runtime): ClaudeInteractiveSession::set_permission_level via \\x1b[Z"
```

---

## Task 7: Pre-seed claude's auto-mode opt-in so the dialog never fires

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude_interactive.rs`

- [ ] **Step 1: Read the existing `ensure_onboarding_complete`**

In `crates/anatta-runtime/src/spawn/claude_interactive.rs`, find `async fn ensure_onboarding_complete(...)`.

- [ ] **Step 2: Add auto-mode markers to the function**

After the existing `obj.insert("hasCompletedOnboarding", ...)` block, add:

```rust
    // Suppress claude code's first-use "Enable auto mode?" dialog. The
    // dialog otherwise fires when the user shift+tabs into Auto for the
    // first time inside a profile that has never accepted it, and would
    // eat the next prompt the same way the theme picker did before
    // `hasCompletedOnboarding` was seeded.
    obj.entry("autoPermissionsNotificationCount".to_string())
        .or_insert_with(|| serde_json::Value::Number(1.into()));
    obj.entry("hasResetAutoModeOptInForDefaultOffer".to_string())
        .or_insert_with(|| serde_json::Value::Bool(true));
```

- [ ] **Step 3: Build clean**

Run: `cargo build -p anatta-runtime --features spawn` — clean.

- [ ] **Step 4: Run tests**

Run: `cargo test -p anatta-runtime --features spawn` — all pre-existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude_interactive.rs
git commit -m "feat(runtime): pre-seed claude's auto-mode opt-in markers"
```

---

## Task 8: Plumb `PermissionLevel` through codex (one-shot + persistent)

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/codex/mod.rs`
- Modify: `crates/anatta-runtime/src/spawn/codex/handshake.rs`
- Modify: `crates/anatta-runtime/src/spawn/codex/launch.rs`
- Modify: `crates/anatta-runtime/src/spawn/codex/persistent.rs`

- [ ] **Step 1: Add `permission_level` field to `CodexLaunch`**

In `crates/anatta-runtime/src/spawn/codex/mod.rs`, find the `pub struct CodexLaunch { ... }` (it's re-exported from a sub-module — check). Add:

```rust
    /// Initial permission level. The mapping to codex's two per-turn
    /// axes (approval_policy, sandbox) and the session-level
    /// `approvals_reviewer` is in `PermissionLevel::codex_policy`.
    pub permission_level: anatta_core::PermissionLevel,
```

(If `CodexLaunch` is defined elsewhere — likely `codex/launch.rs` — edit it there.)

- [ ] **Step 2: Delete the `APPROVAL_POLICY` and `SANDBOX_POLICY` constants**

In `crates/anatta-runtime/src/spawn/codex/mod.rs`, find:

```rust
const APPROVAL_POLICY: &str = "never";
const SANDBOX_POLICY: &str = "danger-full-access";
```

Delete them. The next sub-step replaces their use sites.

- [ ] **Step 3: Update `handshake.rs` to take policy as args**

In `crates/anatta-runtime/src/spawn/codex/handshake.rs`, find the `handshake(...)` function signature. Replace:

```rust
async fn handshake(
    binary_path: &std::path::Path,
    profile: &CodexProfile,
    cwd: &std::path::Path,
    api_key: Option<&str>,
    resume: Option<&str>,
) -> Result<Handshake, SpawnError> {
```

with:

```rust
async fn handshake(
    binary_path: &std::path::Path,
    profile: &CodexProfile,
    cwd: &std::path::Path,
    api_key: Option<&str>,
    resume: Option<&str>,
    policy: anatta_core::CodexPolicy,
) -> Result<Handshake, SpawnError> {
```

Inside the body, find the two `thread/start` / `thread/resume` calls. Replace `APPROVAL_POLICY` with `policy.approval` and `SANDBOX_POLICY` with `policy.sandbox`.

Also: in the `Command::new(binary_path)` argv assembly (look for `cmd.arg("app-server")`), insert before `cmd.arg("app-server")`:

```rust
    if policy.reviewer_armed {
        cmd.arg("-c").arg("approvals_reviewer=auto_review");
    }
```

- [ ] **Step 4: Update callers of `handshake`**

In `crates/anatta-runtime/src/spawn/codex/launch.rs::Launchable for CodexLaunch`, find the `handshake(...).await` call. Add `self.permission_level.codex_policy()` as the new last arg:

```rust
            handshake(
                &self.binary_path,
                &self.profile,
                &self.cwd,
                self.api_key.as_deref(),
                self.resume.as_ref().map(|r| r.as_str()),
                self.permission_level.codex_policy(),
            )
            .await?;
```

Same change in `crates/anatta-runtime/src/spawn/codex/persistent.rs::PersistentCodexSession::open`.

In both files, also fix the `turn/start` request:
- `launch.rs`, find `write_request(&mut stdin, FIRST_TURN_REQUEST_ID, "turn/start", TurnStartParams { ... approval_policy: APPROVAL_POLICY ... })`. Replace `APPROVAL_POLICY` with `self.permission_level.codex_policy().approval`.
- `persistent.rs::send_turn`, same edit using `current_level`'s policy (see Task 9 — for now, route through a local variable so the file builds).

- [ ] **Step 5: Fix call sites in `apps/anatta-cli/src/launch.rs`**

In `build_codex`, add `permission_level: anatta_core::PermissionLevel::Default` to the `CodexLaunch { ... }` struct literal.

- [ ] **Step 6: Fix call sites in tests**

In `crates/anatta-runtime/tests/spawn_e2e.rs::launch_real_codex_*`, add the same field to the `CodexLaunch { ... }` literal.

- [ ] **Step 7: Build + tests**

Run: `cargo build -p anatta-runtime --features spawn` — clean.
Run: `cargo test --workspace --features spawn` — all pre-existing tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/anatta-runtime/src/spawn/codex/ \
        apps/anatta-cli/src/launch.rs \
        crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "feat(runtime): codex CodexLaunch.permission_level + per-turn policy + reviewer flag"
```

---

## Task 9: `PersistentCodexSession::set_permission_level` with close-and-reopen on Auto transitions

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/codex/persistent.rs`

- [ ] **Step 1: Add `current_level` field**

In `crates/anatta-runtime/src/spawn/codex/persistent.rs`, find `pub struct PersistentCodexSession { ... }`. Add:

```rust
    /// Current permission level. Codex's per-turn axes (approval,
    /// sandbox) are flipped via `turn/start` body; the reviewer
    /// (session-level) requires close-and-reopen on transitions across
    /// the Auto boundary — see GitHub issue #22.
    current_level: tokio::sync::Mutex<anatta_core::PermissionLevel>,
```

Initialize from `launch.permission_level` in `open()`.

- [ ] **Step 2: Use `current_level` in `send_turn`**

In `send_turn`, find the `turn/start` write. Get the current level + policy:

```rust
        let policy = {
            let lvl = *self.current_level.lock().await;
            lvl.codex_policy()
        };
```

Replace `approval_policy: APPROVAL_POLICY` (or the local placeholder from Task 8 Step 4) with `approval_policy: policy.approval`. Same for sandbox if it's in the turn body (codex's `turn/start` payload may or may not carry sandbox per the wire shape — check `wire.rs::TurnStartParams`; if sandbox isn't there, the per-handshake sandbox in `thread/start` is the binding one and re-opening the thread is required for sandbox changes too).

If the wire schema does NOT include sandbox in `turn/start`, sandbox changes always require a thread re-open — same close-and-reopen path. Document this with a comment.

- [ ] **Step 3: Implement `set_permission_level` with conditional reopen**

Append to `impl PersistentCodexSession`:

```rust
    /// Switch permission levels.
    ///
    /// Same-axis transitions (between non-Auto levels with the same
    /// sandbox) flip per-turn via `current_level` — next `send_turn`
    /// carries the new policy.
    ///
    /// Cross-axis transitions (across the Auto boundary, or across
    /// sandbox changes) require closing the app-server and reopening
    /// with the right `-c approvals_reviewer=...` flag plus the new
    /// sandbox in the `thread/resume` request. History is preserved
    /// via `thread/resume`.
    pub async fn set_permission_level(
        &mut self,
        target: anatta_core::PermissionLevel,
        launch_template: CodexLaunch,
    ) -> Result<(), SpawnError> {
        let cur = *self.current_level.lock().await;
        if cur == target {
            return Ok(());
        }
        let cur_policy = cur.codex_policy();
        let new_policy = target.codex_policy();
        let needs_reopen = cur_policy.reviewer_armed != new_policy.reviewer_armed
            || cur_policy.sandbox != new_policy.sandbox;

        if !needs_reopen {
            *self.current_level.lock().await = target;
            return Ok(());
        }

        // Reopen: build a new launch carrying the same identity but the
        // new permission level. Caller passes the template (cwd, binary,
        // profile, api_key, resume) so we don't have to keep a copy.
        let mut new_launch = launch_template;
        new_launch.permission_level = target;
        new_launch.resume = Some(CodexThreadId::new(self.thread_id().to_owned()));

        let new_inner = PersistentCodexSession::open(new_launch).await?;
        let old = std::mem::replace(self, new_inner);
        // Best-effort close of the old session; if the close fails the
        // new session is already healthy.
        let _ = old.close().await;
        // current_level on the new session was initialized from
        // new_launch.permission_level, so no further mutation needed.
        Ok(())
    }
```

- [ ] **Step 4: Build + tests**

Run: `cargo build -p anatta-runtime --features spawn` — clean.
Run: `cargo test --workspace --features spawn` — all pre-existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/anatta-runtime/src/spawn/codex/persistent.rs
git commit -m "feat(runtime): PersistentCodexSession::set_permission_level with Auto reopen"
```

---

## Task 10: `Session::set_permission_level` dispatcher

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/session.rs`

- [ ] **Step 1: Add the trait method to `Session`**

In `crates/anatta-runtime/src/spawn/session.rs`, in the `impl Session` block, add:

```rust
    /// Switch the active permission level. Backend-specific:
    /// - Per-turn claude: updates the launch template; next turn picks it up.
    /// - Interactive claude: writes Shift+Tab through the PTY now.
    /// - Codex: per-turn policy flip, or close-and-reopen across Auto.
    pub async fn set_permission_level(
        &mut self,
        target: anatta_core::PermissionLevel,
        // Codex needs the original launch template for reopens.
        codex_launch_template: Option<CodexLaunch>,
    ) -> Result<(), SpawnError> {
        match self {
            Session::Claude(c) => {
                c.template.permission_level = target;
                Ok(())
            }
            Session::ClaudeInteractive(c) => c.set_permission_level(target).await,
            Session::Codex(c) => {
                let tpl = codex_launch_template
                    .ok_or_else(|| SpawnError::Io(std::io::Error::other(
                        "codex permission swap needs launch template",
                    )))?;
                c.inner.set_permission_level(target, tpl).await
            }
        }
    }

    /// Current permission level on this session.
    pub fn permission_level(&self) -> anatta_core::PermissionLevel {
        match self {
            Session::Claude(c) => c.template.permission_level,
            Session::ClaudeInteractive(c) => c.permission_level(),
            // For codex, we'd need an async lock — return a synchronous
            // approximation via `try_lock`. Acceptable for UI; the
            // authoritative value is at send-turn time.
            Session::Codex(c) => {
                use tokio::sync::Mutex;
                let g = Mutex::try_lock(&c.inner.current_level);
                g.map(|l| *l).unwrap_or(anatta_core::PermissionLevel::Default)
            }
        }
    }
```

- [ ] **Step 2: Build**

Run: `cargo build -p anatta-runtime --features spawn` — clean.

- [ ] **Step 3: Run tests**

Run: `cargo test -p anatta-runtime --features spawn` — pre-existing tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/anatta-runtime/src/spawn/session.rs
git commit -m "feat(runtime): Session::set_permission_level dispatcher"
```

---

## Task 11: REPL — bind Shift+Tab to `ReadOutcome::CyclePermission` (TDD)

**Files:**
- Create: `apps/anatta-cli/src/chat/permission_hotkey.rs`
- Modify: `apps/anatta-cli/src/chat/input.rs`
- Modify: `apps/anatta-cli/src/chat/mod.rs`

- [ ] **Step 1: Create the hotkey module**

Create `apps/anatta-cli/src/chat/permission_hotkey.rs`:

```rust
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
```

- [ ] **Step 2: Wire the binding into `InputReader`**

Edit `apps/anatta-cli/src/chat/input.rs`. Replace the imports + struct + `new` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;

use rustyline::{DefaultEditor, EventHandler, KeyCode, KeyEvent, Modifiers};
use rustyline::error::ReadlineError;

use super::ChatError;
use super::permission_hotkey::CyclePermissionFlag;

pub(crate) struct InputReader {
    editor: DefaultEditor,
    history_path: PathBuf,
    cycle_flag: CyclePermissionFlag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    Line(String),
    Eof,
    Interrupted,
    /// User pressed Shift+Tab — cycle to the next permission level.
    CyclePermission,
}

impl InputReader {
    pub(crate) fn new(anatta_home: &std::path::Path) -> Result<Self, ChatError> {
        let mut editor = DefaultEditor::new().map_err(|e| ChatError::Readline(e.to_string()))?;
        let cycle_flag = CyclePermissionFlag::new();
        editor.bind_sequence(
            KeyEvent(KeyCode::BackTab, Modifiers::NONE),
            EventHandler::Conditional(Box::new(cycle_flag.handler())),
        );
        let history_path = anatta_home.join("chat_history");
        let _ = editor.load_history(&history_path);
        Ok(Self {
            editor,
            history_path,
            cycle_flag,
        })
    }

    pub(crate) fn read_prompt(&mut self) -> ReadOutcome {
        match self.editor.readline("> ") {
            Ok(line) => {
                // If Shift+Tab was pressed, the line is whatever was in
                // the buffer (likely empty) — discard and surface the
                // cycle event.
                if self.cycle_flag.take() {
                    return ReadOutcome::CyclePermission;
                }
                let trimmed = line.trim().to_owned();
                if !trimmed.is_empty() {
                    let _ = self.editor.add_history_entry(&trimmed);
                }
                ReadOutcome::Line(trimmed)
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(ReadlineError::Interrupted) => ReadOutcome::Interrupted,
            Err(other) => {
                eprintln!("[anatta] input error: {other}");
                ReadOutcome::Eof
            }
        }
    }

    pub(crate) fn save_history(&mut self) {
        let _ = self.editor.save_history(&self.history_path);
    }
}
```

Hold onto `cycle_flag` in the struct so the handler's `Arc` stays alive.

- [ ] **Step 3: Register the new module**

In `apps/anatta-cli/src/chat/mod.rs`, add `mod permission_hotkey;` near the existing `mod input;` line.

- [ ] **Step 4: Build**

Run: `cargo build -p anatta-cli` — clean.

If rustyline's `EventHandler::Conditional` takes `Box<dyn ConditionalEventHandler + Send + Sync>` rather than `Box<dyn ConditionalEventHandler>`, add the marker bounds to `CyclePermissionHandler`. Adjust as the compiler dictates.

- [ ] **Step 5: Commit**

```bash
git add apps/anatta-cli/src/chat/permission_hotkey.rs \
        apps/anatta-cli/src/chat/input.rs \
        apps/anatta-cli/src/chat/mod.rs
git commit -m "feat(cli): bind Shift+Tab in rustyline → ReadOutcome::CyclePermission"
```

---

## Task 12: Chat runner — handle `CyclePermission` + render the current level

**Files:**
- Modify: `apps/anatta-cli/src/chat/runner.rs`

- [ ] **Step 1: Locate the read loop**

In `apps/anatta-cli/src/chat/runner.rs`, find the main loop:

```rust
    let result: Result<(), ChatError> = loop {
        renderer.pre_prompt();
        match input.read_prompt() {
            ReadOutcome::Eof | ReadOutcome::Interrupted => { ... }
            ReadOutcome::Line(s) if s.is_empty() => continue,
            ReadOutcome::Line(s) if s.starts_with('/') => { ... }
            ReadOutcome::Line(prompt) => { ... }
        }
    };
```

- [ ] **Step 2: Add the `CyclePermission` arm**

Insert the new arm at the top of the match (just below `ReadOutcome::Eof | ReadOutcome::Interrupted`):

```rust
            ReadOutcome::CyclePermission => {
                let new_level = session.permission_level().next();
                // Codex reopen needs a fresh launch template; rebuild
                // from the stored profile + current cwd.
                let codex_template = match profile.backend {
                    anatta_store::profile::BackendKind::Codex => match launch::build_launch(
                        &profile,
                        cwd.clone(),
                        backend_session_id.clone(),
                        cfg,
                    ) {
                        Ok(anatta_runtime::spawn::BackendLaunch::Codex(l)) => Some(l),
                        _ => None,
                    },
                    _ => None,
                };
                if let Err(e) = session.set_permission_level(new_level, codex_template).await {
                    eprintln!("✗ permission swap failed: {e}");
                    continue;
                }
                renderer.permission_changed(new_level);
                continue;
            }
```

- [ ] **Step 3: Add `Renderer::permission_changed`**

In `apps/anatta-cli/src/chat/render.rs` (or wherever the `Renderer` struct lives — find via `grep -rn "fn pre_prompt" apps/anatta-cli/src/chat`), add:

```rust
    pub(crate) fn permission_changed(&self, level: anatta_core::PermissionLevel) {
        println!("⏵⏵ permission level: {}", level.label());
    }
```

Also update `pre_prompt` (if it exists) to print the current level as part of the status line:

```rust
    pub(crate) fn pre_prompt(&self, level: anatta_core::PermissionLevel) {
        println!("⏵⏵ {}  ·  shift+tab to cycle", level.label());
    }
```

Update its caller in `runner.rs` to pass the session's current level:

```rust
        renderer.pre_prompt(session.permission_level());
```

- [ ] **Step 4: Build**

Run: `cargo build -p anatta-cli` — clean.

- [ ] **Step 5: Run all tests + manual smoke**

Run: `cargo test --workspace --features spawn` — all pre-existing tests pass.

Manual smoke (real claude): `./target/release/anatta chat new test-perm-cycling --profile claude-IE55KzhO`. In the chat, type a normal turn ("hi"), then press Shift+Tab and confirm the status line above the next `> ` updates to "accept edits". Repeat a few cycles. Then send another prompt and verify claude responds normally with the new mode.

- [ ] **Step 6: Commit**

```bash
git add apps/anatta-cli/src/chat/runner.rs apps/anatta-cli/src/chat/render.rs
git commit -m "feat(cli): handle CyclePermission in chat REPL + render current level"
```

---

## Task 13: Plumb default `PermissionLevel` through CLI launch builder

**Files:**
- Modify: `apps/anatta-cli/src/launch.rs`

- [ ] **Step 1: Verify all `Launch` literals carry `permission_level`**

Tasks 4, 5, 8 added `permission_level: anatta_core::PermissionLevel::Default` to the literals in `build_claude`, `build_claude_interactive`, and `build_codex`. Verify with `cargo build -p anatta-cli`; if it builds, the field is wired.

- [ ] **Step 2: Optionally surface `--permission-level <level>` at the CLI**

(Optional — can be deferred.) Add to `SendArgs` (`apps/anatta-cli/src/send.rs`) and `ChatCommand::New` (`apps/anatta-cli/src/chat/mod.rs`):

```rust
    /// Initial permission level (cycle with Shift+Tab in chat).
    #[arg(long, value_enum, default_value = "default")]
    permission_level: PermissionLevelArg,
```

where `PermissionLevelArg` is a `clap::ValueEnum` wrapper around `PermissionLevel`.

If you do this, thread it through to `build_launch` as a new argument and override the default in `build_claude_interactive` / `build_codex` / `build_claude`. **For this plan's MVP, skip the CLI flag and accept `Default` as the only entry point.** Cycling via Shift+Tab covers the in-session need.

- [ ] **Step 3: Commit (no-op if nothing changed)**

If you didn't add the CLI flag, this task has no commit. Skip.

---

## Task 14: Final integration smoke + documentation

**Files:**
- Modify: `docs/superpowers/plans/2026-05-15-permission-level-cycling.md` (this file — add a "Risks & follow-ups" section if not already present)

- [ ] **Step 1: Run the full test suite and CI emulation**

Run:
```
cargo fmt --all -- --check
cargo clippy --workspace --features spawn -- -D warnings
cargo test --workspace --features spawn
```

All three must be clean.

- [ ] **Step 2: Real-claude smoke**

For each of the three claude profiles, run:

```
./target/release/anatta send <profile-id> "Say only OK and nothing else"
```

Verify each returns "OK" within ~10 s. This confirms the new `--permission-mode default` argv (replacing the prior `dontAsk`) doesn't break the per-turn path.

- [ ] **Step 3: Real-codex smoke**

```
./target/release/anatta send codex-Suv1ZBG6 "Say only OK"
```

Verify response.

- [ ] **Step 4: Manual interactive smoke**

```
./target/release/anatta chat new test-perm-cycling --profile claude-IE55KzhO --cwd /tmp
```

In the chat:
1. Send "hi" — verify normal response, status line shows `default`.
2. Press Shift+Tab — status line updates to `accept edits`.
3. Press Shift+Tab three more times to reach `bypass all`.
4. Send another short turn — verify claude responds without errors.
5. Press Shift+Tab to reach `plan`. Send a "what is this file" prompt — claude should run a read-only tool (e.g. Read) and respond, without attempting any edits.
6. `/exit` to close cleanly.

Document any unexpected behavior as a follow-up issue rather than blocking the merge.

- [ ] **Step 5: Commit a CHANGELOG / docs note (if the repo has one)**

If `docs/` or `README.md` mentions permission modes, update them to reference the new Shift+Tab UX. Otherwise skip.

- [ ] **Step 6: Open the PR**

```bash
gh pr create --title "feat: shift+tab permission-level cycling across claude + codex backends" \
    --body "Implements PermissionLevel cycling per the design in #22. ..."
```

---

## Risks & follow-ups (deferred; not part of this plan)

1. **Claude's empirical cycle order may differ.** I assumed `default → acceptEdits → auto → bypassPermissions → plan → default`. If a real trace shows `auto` is between `bypassPermissions` and `plan`, or that `dontAsk` is in the user cycle, the `shift_tab_count` math drifts. Mitigation: a Step in Task 6 should add a manual trace step before shipping. Plan B: instead of computing distance, always over-cycle (write `\x1b[Z` × `CYCLE.len() + index_of(target)`) to deterministically land regardless of starting position.

2. **`dontAsk` is gone from the user surface.** We removed the special-case `dontAsk` argv. With `Default` as the new initial level, any sub-task that relied on `dontAsk`'s permissive denial behavior (notably the `AskUserQuestion` graceful failure mode the user encountered earlier today) reverts to claude's normal permission flow — which we can't display because anatta sinks the TUI. Net effect: tools that need user prompts will hang in `Default` mode. Workaround: ship with `BypassAll` as the per-session default (instead of `Default`) until the tool-result round-trip is implemented. **Worth deciding before merge** — see Task 13 Step 2 for where to flip the default.

3. **`approvals_reviewer=auto_review` interaction with `approval_policy=never`.** Per the docs, the reviewer "applies when approvals are interactive." With our `BypassAll` → `(never, danger-full-access, reviewer off)`, no review fires. With `Auto` → `(on-request, workspace-write, reviewer on)`, reviewer fires. The boundary handling in `set_permission_level` is correct, but we should verify the actual codex behavior matches the docs with one E2E run.

4. **Per-session reopen latency on codex Auto transitions.** Empirically measure once after shipping; if >500 ms, follow up with the warm-pool optimization noted in #22.

5. **Multiple `\x1b[Z` writes racing the bottom-bar update.** If we write 4 Shift+Tabs and then immediately a prompt, claude's TUI may still be cycling when our prompt arrives. Risk: the prompt lands on a partially-rendered state. Mitigation: insert a tiny `tokio::time::sleep(Duration::from_millis(50))` between the cycle writes and the prompt in `send_turn`'s caller — only when a cycle was performed in the same loop iteration. Not part of this plan; tackle if seen in the manual smoke.

---

## Self-Review

**Spec coverage:**

| Requirement | Task |
|---|---|
| `PermissionLevel` enum in `anatta-core` | Task 1 |
| Claude `--permission-mode` mapping | Task 2 |
| Codex `(approval, sandbox, reviewer)` mapping | Task 3 |
| `ClaudeLaunch.permission_level` (per-turn) | Task 4 |
| `ClaudeInteractiveLaunch.permission_level` (PTY) + tracker | Task 5 |
| `\x1b[Z` cycling in claude PTY | Task 6 |
| Pre-seed auto-mode opt-in | Task 7 |
| Codex `CodexLaunch.permission_level` + per-turn `turn/start` policy + reviewer CLI flag | Task 8 |
| Codex close-and-reopen on Auto transitions | Task 9 |
| `Session::set_permission_level` dispatcher | Task 10 |
| Shift+Tab rustyline binding | Task 11 |
| Chat REPL handles `CyclePermission` + renders level | Task 12 |
| CLI default + optional `--permission-level` flag | Task 13 |
| Smoke + docs | Task 14 |

All requirements covered.

**Placeholder scan:**

- "(Optional — can be deferred.)" in Task 13 — explicitly marked optional with the recommendation to skip for MVP. Not a placeholder.
- "Fix each by adding..." in Task 4 Step 4 — refers to specific files (`apps/anatta-cli/src/launch.rs`, `crates/anatta-runtime/tests/spawn_e2e.rs`); concrete enough.
- No "TBD" / "implement later" / "appropriate error handling" patterns.

**Type consistency:**

- `PermissionLevel` enum: same five variants used everywhere (`Default`, `AcceptEdits`, `Auto`, `BypassAll`, `Plan`).
- `CodexPolicy` struct: `approval`, `sandbox`, `reviewer_armed` (consistent across tasks 3, 8, 9).
- `set_permission_level` signatures:
  - `ClaudeInteractiveSession::set_permission_level(&self, target) -> Result<(), SpawnError>` — Task 6.
  - `PersistentCodexSession::set_permission_level(&mut self, target, launch_template: CodexLaunch) -> Result<(), SpawnError>` — Task 9.
  - `Session::set_permission_level(&mut self, target, codex_launch_template: Option<CodexLaunch>) -> Result<(), SpawnError>` — Task 10.
  - All consistent.
- `shift_tab_count(from, to) -> usize` — Task 6.
- `Renderer::pre_prompt(level)` — added in Task 12 (previously `pre_prompt()` with no args; the migration is in-scope for Task 12 Step 3).

One drift: Task 10 references `c.template.permission_level` for `Session::Claude(c)`. Verify the field name on `ClaudeSession.template` matches what Task 4 added. Both refer to `ClaudeLaunch.permission_level`, so `c.template.permission_level` is consistent.

No naming drift detected.
