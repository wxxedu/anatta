# `anatta` slash command dispatcher — unified registry, tier 1

**Status**: design draft, post round-1 codex audit
**Date**: 2026-05-12
**Owner**: wxx
**Tracking issue**: wxxedu/anatta#1 (PR-A) under wxxedu/anatta#5

## Problem

Slash command handling today lives ad-hoc in `apps/anatta-cli/src/chat/slash.rs`:
a single `handle(line, profile, cfg)` function `match`es on the first word,
dispatches to a handler, and returns one of three `SlashOutcome` variants
(`Continue` / `Exit` / `SwapProfile`). The chat runner at
`apps/anatta-cli/src/chat/runner.rs:185-260` interprets the outcome and
executes side effects.

This works fine for the four commands that exist (`/profile`, `/exit`, `/quit`,
`/help`) but doesn't scale:

1. **Three upcoming PRs** (skills, MCP, plugins) each want to register their
   own commands. Without a real registry we'd fork the dispatch logic three
   times.
2. **Subcommands are sketched ad-hoc**: `/profile` accepts no subcommands today,
   but `/skill list`, `/mcp add foo --command bar`, `/plugin list` are coming.
   The current single-word `match` doesn't support hierarchy.
3. **TAB completion and help generation** require structured metadata
   (names, descriptions, subcommands) that don't exist as data anywhere.
4. **anatta-runtime owns the data** that most upcoming commands operate on
   (skill listings, MCP configs, plugin discovery), but command definitions
   are stuck in `apps/anatta-cli/`. Reusing them from a future TUI / web
   frontend would require duplicating dispatch.

PR-A is a **pure refactor**: introduce a structured registry, migrate the four
existing commands into it, leave hooks for PR-B/C/D to register their own
commands. Externally visible behavior is unchanged.

## Non-goals

- New user-facing commands (those land in PR-B/C/D).
- Plugin command discovery / `<plugin>:<cmd>` namespace (PR-D).
- Backend escape-hatch namespace `/native:<x>` (no real use case yet; revisit
  after PR-D).
- Rustyline integration for TAB completion (only API surface in PR-A).
- Rich help formatting beyond what's needed to replace today's `/help`.

## Architecture

### Three boundaries

```
┌─────────────────────────────────────────────────────────┐
│ apps/anatta-cli/src/chat/dispatch/                      │
│   Dispatcher: parse line → walk registry → invoke       │
│               handler → render CommandResult            │
│   Registers:  CLI-only commands (/exit, /quit, /help,   │
│               /profile-the-picker)                      │
└─────────────────────────────────────────────────────────┘
                          │
                          │ uses
                          ▼
┌─────────────────────────────────────────────────────────┐
│ crates/anatta-runtime/src/commands/                     │
│   Types:      CommandNode, Handler, CommandResult,      │
│               CommandCtx, CommandError                  │
│   Exports:    fn registry() -> Vec<CommandNode>         │
│   (PR-A populates with nothing; PR-B/C/D fill in)       │
└─────────────────────────────────────────────────────────┘
                          │
                          │ uses
                          ▼
┌─────────────────────────────────────────────────────────┐
│ crates/anatta-store, anatta-runtime business APIs       │
│   list_profiles, swap_to_profile, etc.                  │
└─────────────────────────────────────────────────────────┘
```

**Why types in `anatta-runtime`, not `anatta-cli`**: the runtime is the
architectural target. Future TUI / web frontends import `runtime::commands`
and apply their own rendering. Putting types in CLI signals "CLI's internal
abstraction" which is wrong.

**Why no separate `anatta-commands` crate**: one crate dependency is enough.
Splitting would be premature.

### New dependency: `anatta-runtime → anatta-store`

PR-A adds `anatta-store = { path = "../anatta-store" }` to
`crates/anatta-runtime/Cargo.toml`'s `[dependencies]` (unconditional, not
behind a feature). Justification:

- `CommandCtx` needs `Store` and `ProfileRecord`, both defined in `anatta-store`.
- `CommandResult::SwapProfile` wraps `ProfileRecord`.
- The only current consumer of `anatta-runtime` is `apps/anatta-cli`, which
  already depends on `anatta-store`. No external consumer is forced to pull
  sqlx by this change.
- Layering remains acyclic: `anatta-store` (data) ← `anatta-runtime` (logic + commands)
  ← `apps/anatta-cli` (frontend).

The runtime crate's top-level doc comment should be updated from "subprocess
runtime: spawn, stream IO, supervise lifecycle" to also cover "command
registry and business logic for anatta state". This is a comment change in
`crates/anatta-runtime/src/lib.rs` plus the Cargo.toml `description` field.

### Namespace

Only one namespace in PR-A: **bare `/foo`** = anatta builtin.

Deferred:
- `/<plugin>:<cmd>` — added in PR-D when plugin discovery exists.
- `/native:<x>` — no use case identified. `claude --print` / `codex exec`
  modes don't expose backend slash commands, so there's nothing to passthrough
  to. Revisit after PR-D; may never be needed.

Unknown bare commands print `unknown command: /foo (try /help)` and continue.
**No fall-through to backend.** Future anatta builtins might collide with
backend commands; explicit failure prevents silent behavior change.

### Data shapes

All types live in `crates/anatta-runtime/src/commands/mod.rs`:

```rust
pub struct CommandNode {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub args_hint: String,                // shown in help; "" for groups
    pub handler: Option<Arc<dyn CommandHandler>>, // None = pure group
    pub children: Vec<CommandNode>,       // empty = pure leaf
}

#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    /// `args` are the tokens remaining after this node was matched
    /// (i.e., not consumed by walking into children). For a pure leaf
    /// at the top of the tree, `args` is everything after the command
    /// name. For a group with a default handler, `args` is whatever
    /// didn't match any child.
    async fn run(
        &self,
        args: &[&str],
        ctx: &CommandCtx,
    ) -> Result<CommandResult, CommandError>;
}

pub struct CommandCtx {
    pub store: Store,                              // cheap clone (Arc<SqlitePool> inside)
    pub current_profile: ProfileRecord,            // cloned per dispatch — small
    pub anatta_home: PathBuf,
    pub registry: Arc<RegistrySnapshot>,           // for /help; see below
}

/// Read-only view of the registry's metadata. Built once after all
/// commands register. Contains no handlers — just names, descriptions,
/// children — enough to format help and complete tab.
pub struct RegistrySnapshot {
    pub roots: Vec<NodeMeta>,
}

pub struct NodeMeta {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub args_hint: String,
    pub children: Vec<NodeMeta>,
}

pub enum CommandResult {
    /// Command completed; no display, no side effect. Used by handlers
    /// that already printed (CLI-side handlers using dialoguer) or that
    /// have nothing to say.
    Continue,

    /// Print plain text. Runtime commands typically return this.
    Text(String),

    /// Print as aligned table. Row 0 is headers.
    Table { rows: Vec<Vec<String>> },

    /// Exit the chat REPL gracefully (`/exit`, `/quit`).
    /// CLI-only outcome; runtime commands won't return this.
    Exit,

    /// User picked a profile to swap to. CLI runner takes over
    /// from here (absorb → close segment → open new → render → spawn).
    /// Boxed to keep enum variant sizes balanced (matches current
    /// `SlashOutcome::SwapProfile` clippy-driven shape).
    SwapProfile(Box<ProfileRecord>),
}

pub enum CommandError {
    /// Handler-level domain error (e.g., can't read skill dir).
    /// CLI prints `error: {msg}` and continues.
    Domain(String),
}

pub enum RegistryError {
    DuplicateName(String),
    AliasCollidesWithName(String),
    EmptyName,
}
```

**Why trait-based handler, not `Fn`-closure type**: an HRTB closure type
like `Arc<dyn for<'a> Fn(&'a [&'a str], &'a CommandCtx) -> BoxFuture<'a, ...>>`
compiles but is painful at call sites and incompatible with `async fn`
bodies without a `Box::pin` adapter at every registration. `async_trait`
is already a workspace dep (used by `Launchable` in
`crates/anatta-runtime/src/spawn/mod.rs:47-51`). Reusing it is consistent.
Boilerplate per command: a unit struct + `impl CommandHandler`. Acceptable.

**Why owned `Store` / `ProfileRecord` / `PathBuf` in `CommandCtx`, not `&'a`**:
`Store` is `Clone` over an internal `Arc<SqlitePool>` (`crates/anatta-store/src/lib.rs:26-28`),
so cloning is cheap. `ProfileRecord` is a small data struct. Owned context
removes all lifetime parameters from the trait, which both simplifies the
public API and is required for `#[async_trait]` to generate clean code.
One `CommandCtx` is constructed per slash command invocation; this is not
a hot path.

**Why `RegistrySnapshot` in `CommandCtx`**: `/help` must walk the registry
to format its output. If `/help` is a registered command, the handler
cannot hold a reference to the dispatcher it lives inside (would be a
self-reference). Solution: build the registry, then freeze the metadata
into an `Arc<RegistrySnapshot>` (no handlers, just names/descriptions/children),
expose it through `CommandCtx`. The snapshot includes `/help` itself so
its own listing in `/help`'s output is correct.

**Why no `NeedsInteractive` variant**: I/O orchestration (dialoguer pickers,
`$EDITOR` spawning) belongs in CLI-side handlers. They call runtime APIs
directly, render, call back. Pushing interactive flows through the enum
would require typed variants per flow and serializable closures — neither
worth the complexity.

### Dispatch algorithm

```text
fn walk(node: &CommandNode, tokens: &[&str], ctx) -> Result<CommandResult> {
    if tokens.is_empty() {
        match &node.handler {
            Some(h) => return h(&[], ctx).await,
            None => {
                // Pure group with no args: print children listing
                return Ok(Text(format_group_help(node)));
            }
        }
    }

    let head = tokens[0];
    if let Some(child) = node.children.iter().find(|c|
        c.name == head || c.aliases.contains(head)
    ) {
        return walk(child, &tokens[1..], ctx).await;
    }

    // head didn't match any child
    match &node.handler {
        Some(h) => h(tokens, ctx).await,   // leaf w/ args (or group default w/ args)
        None    => Ok(Text(format!(
            "unknown subcommand: {head} (try /{} for options)", node.name
        ))),
    }
}
```

Top level: `Registry { roots: Vec<CommandNode>, snapshot: Arc<RegistrySnapshot> }`.

Parse the line:

```rust
fn parse(line: &str) -> Option<Vec<&str>> {
    // strip_prefix, not trim_start_matches: `//help` becomes `/help` (one
    // strip) but the next call would already have non-slash content, so
    // tokens[0] is "/help" which doesn't match any root and falls through
    // to "unknown command". This preserves today's behavior in
    // `apps/anatta-cli/src/chat/slash.rs:42-52` where `//help` is unknown.
    let trimmed = line.strip_prefix('/')?.trim();
    if trimmed.is_empty() { return None; }
    Some(trimmed.split_whitespace().collect())
}
```

Then `walk(root_matching(tokens[0]), &tokens[1..], ctx)`. Unknown top-level
prints `unknown command: /{head} (try /help)`.

### `/profile` handler specification

`/profile` is a leaf with a default handler today (no children). In future,
it may grow children (e.g., `/profile list`, `/profile show <id>`). The
handler must distinguish:

| `args` | Behavior |
|---|---|
| `[]` | Open dialoguer picker (today's behavior) |
| `["list"]` etc. matching a known child | Never reaches the handler — dispatcher walks into the child first |
| Any other non-empty `args` | Return `Err(CommandError::Domain(format!("unknown subcommand: {}; type /profile for the picker, /help profile for options", args[0])))` |

The third case is the key one codex flagged: without explicit handling,
`/profile blah` would silently open the picker. The handler must check
`args.is_empty()` before doing picker work and reject otherwise.

This pattern (default handler that only runs on empty args, otherwise
rejects) is sketched here for `/profile`; PR-B/C/D handlers that follow
the same pattern should crib from it.

### Behavior table

| Input | Walk | Result |
|---|---|---|
| `/profile` | profile has handler, tokens empty | invoke handler with `[]` → returns `SwapProfile(...)` or `Continue` |
| `/profile list` | walks to `list` child | invoke list's handler (PR-A: doesn't exist yet; PR-?) |
| `/profile blah` | profile has handler, blah unmatched | invoke profile handler with `["blah"]` — handler decides |
| `/mcp` | mcp has no handler (pure group, PR-C+), no tokens | print group help |
| `/mcp blah` | mcp pure group, blah unmatched | print "unknown subcommand: blah (try /mcp for options)" |
| `/exit` | exit leaf, no tokens | handler returns `Exit` |
| `/foo` | foo not in roots | print "unknown command: /foo (try /help)", Continue |
| `/help skill` | help handler, args = `["skill"]` | handler walks registry to skill, formats subtree |

### Registry construction

```rust
// crates/anatta-runtime/src/commands/mod.rs
pub fn registry() -> Vec<CommandNode> {
    // PR-A: empty.
    // PR-B: pushes the skill subtree.
    // PR-C: pushes the mcp subtree.
    // PR-D: pushes the plugin subtree.
    Vec::new()
}
```

```rust
// apps/anatta-cli/src/chat/dispatch/mod.rs
pub fn build_dispatcher() -> Result<Dispatcher, RegistryError> {
    let mut nodes = anatta_runtime::commands::registry();
    nodes.extend(repl_only::commands());      // /exit, /quit, /help
    nodes.extend(interactive::commands());    // /profile (picker handler)
    Dispatcher::from_roots(nodes)
}
```

`Dispatcher::from_roots(nodes)` returns `Result<Dispatcher, RegistryError>`.
It walks the tree once to:
1. Detect duplicate names / alias-vs-name collisions (returns `Err`).
2. Build the `Arc<RegistrySnapshot>` (handler-stripped metadata view) that
   gets handed to every `CommandCtx`.
3. Store the roots + snapshot in the `Dispatcher` value.

Built **once per chat session**, immediately after profile resolution.
CLI calls `.expect("registry build")` because a malformed registry is a
programmer error caught in dev. Tests can assert the `Err` variants
structurally. PR-D may need to rebuild on profile swap if plugin commands
are project-scoped; that's PR-D's concern, and rebuilding is supported
because nothing pins the `Dispatcher` value.

**Why `Result`, not panic**: codex flagged that panicking from a library
type's constructor mixes concerns. CLI startup wants fail-fast (`.expect`);
unit tests want to assert specific error variants without `catch_unwind`.
`Result` satisfies both.

### TAB completion API

```rust
impl Dispatcher {
    pub fn complete(&self, tokens: &[&str]) -> Vec<String> {
        // Walk as far as tokens match exactly.
        // Return child names (and aliases) whose prefix matches the
        // unmatched portion. If we land on a leaf with a handler,
        // return [] (handler-internal completion is the handler's job).
    }
}
```

Not wired to rustyline in PR-A. The API exists; future work surfaces it.

## Migration plan

Four steps. Each step compiles and preserves the existing E2E smoke test
pass. Old `chat/slash.rs` stays wired into `runner.rs` until the final step
flips the switch.

1. **Add types + empty registry to `anatta-runtime`.**
   - Add `anatta-store` to `crates/anatta-runtime/Cargo.toml` `[dependencies]`.
   - New module `crates/anatta-runtime/src/commands/mod.rs` defining
     `CommandNode`, `CommandHandler`, `CommandResult`, `CommandCtx`,
     `CommandError`, `RegistryError`, `RegistrySnapshot`, `NodeMeta`, and
     `pub fn registry() -> Vec<CommandNode> { Vec::new() }`.
   - Update `anatta-runtime` crate doc comment + Cargo `description`.
   - No callers yet. CI green.

2. **Add CLI dispatcher + all four handlers in parallel.** Nothing wired
   into `runner.rs` yet — `chat/slash.rs` still owns dispatch.
   - New `apps/anatta-cli/src/chat/dispatch/mod.rs`: `Dispatcher`,
     `from_roots`, `dispatch(line, ctx)`, `complete(tokens)`.
   - New `chat/dispatch/builtins/`:
     - `help.rs` — handler walks `ctx.registry: Arc<RegistrySnapshot>`,
       formats. Special case `/help <topic>`: walks the snapshot to a
       named subtree.
     - `exit.rs` — single handler, name `exit`, alias `quit`. Returns
       `CommandResult::Exit`.
     - `profile.rs` — handler. Empty args → picker (returns `SwapProfile`
       or `Continue`). Non-empty args → `Err(Domain("unknown subcommand"))`.
   - `chat/dispatch/build.rs` exports `build_dispatcher(...)` that
     assembles roots + calls `Dispatcher::from_roots`.
   - Unit tests for each handler.
   - Integration tests for parser + walk + alias.
   - Existing `chat/slash.rs` untouched. Existing E2E smoke still passes
     against the old path. CI green.

3. **Flip `runner.rs` to the new dispatcher; delete old code.**
   - `runner.rs:185-260` block changes from `slash::handle(...)` match to:
     ```rust
     ReadOutcome::Line(s) if s.starts_with('/') => {
         let ctx = CommandCtx { store, current_profile, anatta_home, registry };
         match dispatcher.dispatch(&s, &ctx).await {
             Ok(CommandResult::Continue) => continue,
             Ok(CommandResult::Text(t)) => { eprintln!("{t}"); continue; }
             Ok(CommandResult::Table { rows }) => { print_table(rows); continue; }
             Ok(CommandResult::Exit) => break Err(ChatError::InputClosed),
             Ok(CommandResult::SwapProfile(p)) => {
                 let new_profile = *p;
                 // ... existing 50-line orchestration block, unchanged
             }
             Err(CommandError::Domain(m)) => { eprintln!("error: {m}"); continue; }
         }
     }
     ```
   - `dispatcher` is constructed once at session start, before the loop.
   - Delete `apps/anatta-cli/src/chat/slash.rs`. Remove `mod slash;` from
     `chat/mod.rs`.
   - E2E smoke test exercises the new path end-to-end.

4. **Verification pass.**
   - Run `cargo test -p anatta-runtime -p anatta-cli`.
   - Run `python3 /tmp/anatta_e2e_smoke.py` (or repo-tracked equivalent).
   - Manually exercise `/help`, `/help help`, `/exit`, `/quit`, `/profile`
     (same backend), `/profile` (cross-backend warning), `/foo` (unknown).

## Test plan

Unit tests in `apps/anatta-cli/src/chat/dispatch/`:

- `parse_strips_leading_slash`
- `parse_returns_none_for_empty_after_slash`
- `walk_pure_leaf_no_args` — handler invoked with `[]`
- `walk_pure_leaf_with_args` — handler invoked with passed tokens
- `walk_pure_group_no_args` — returns group help
- `walk_pure_group_with_unknown_arg` — returns "unknown subcommand"
- `walk_group_with_default_handler_unknown_arg` — handler invoked with token as arg
- `walk_alias_resolves_same_handler`
- `walk_three_levels_deep` — `/a b c` resolves
- `complete_top_level_with_empty_prefix` — returns all root names
- `complete_partial_prefix` — filters
- `complete_after_group` — returns children of group
- `duplicate_name_returns_err` — `Dispatcher::from_roots` → `Err(RegistryError::DuplicateName(_))`
- `alias_collides_with_name_returns_err` — same shape, different variant
- `empty_name_returns_err` — guards against registration bugs
- `parse_double_slash_returns_one_strip` — `//help` becomes one-element tokens `["/help"]`, dispatched as unknown
- `profile_handler_rejects_unknown_arg` — `/profile foo` returns `Err(Domain)`
- `profile_handler_empty_args_invokes_picker` — covered via a small fake `Store` or `#[cfg(test)]` shim

E2E (existing smoke test at `/tmp/anatta_e2e_smoke.py`):
- Unchanged. Should pass before and after. Verifies `/profile` picker and
  Ctrl-D-as-`/exit` still work via the new dispatcher.

## Open questions

None remaining. Round-1 codex audit identified 9 issues (1 critical, 2 high,
4 medium, 2 low); all have been addressed in this revision:

| # | Issue | Resolution |
|---|---|---|
| 1 | `CommandCtx` would pull `anatta-store` into `anatta-runtime` | Accept: add `anatta-store` as runtime dep; document layering |
| 2 | `/help` self-reference | `RegistrySnapshot` handed via `CommandCtx`; `/help` reads metadata only |
| 3 | Migration steps not independently green | Collapsed 6 steps into 4; old `slash.rs` lives until final step |
| 4 | HRTB `Fn`-closure Handler type painful | `#[async_trait] trait CommandHandler` instead |
| 5 | `Store` not `Arc`'d in `Config` | `CommandCtx` holds owned `Store` (cheap Clone), no lifetime params |
| 6 | `/profile blah` semantics undefined | Handler rejects non-empty args explicitly |
| 7 | `from_roots` panic vs `Result` | Returns `Result<Dispatcher, RegistryError>`; CLI `.expect()`s |
| 8 | `strip_prefix` vs `trim_start_matches` for `//help` | Use `strip_prefix` to preserve current behavior |
| 9 | `SwapProfile` boxing | Already correct, kept boxed |

Awaiting round-2 codex audit on this revised spec.

## Out of scope for PR-A (recorded for follow-up)

- **Runtime commands list is empty in PR-A.** First real entries land in PR-B.
- **Rustyline TAB completion wiring** — API exists, integration deferred.
- **Help formatting beyond text dump** — basic listing for now; richer
  paging / colouring later if needed.
- **Persistent dispatcher state** — registry built per session; no cache
  between sessions. Fine, commands are static metadata.
- **Plugin command refresh on profile swap** — PR-D concern.
