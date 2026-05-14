# `anatta` conversation segments — multi-profile continuity, tier 1

**Status**: design accepted, ready to implement
**Date**: 2026-05-12 (revised after codex audit + JSONL self-review)
**Owner**: wxx

## Problem

Today a conversation is bound to a single profile via `conversations.profile_id`,
and the on-disk JSONL written by `claude` / `codex` lives under a *shared*
`projects/` directory that all profiles' `CLAUDE_CONFIG_DIR` symlink into
(see `crates/anatta-runtime/src/profile/claude.rs:37-43`). Two consequences:

1. There is no record of **which profile produced which span of history**.
   `conversations.profile_id` records the *current* profile only.
2. Even though same-family swap "just works" via the shared `projects/`,
   **cross-family swap is unsafe**: a session that recorded `thinking` blocks
   under a 3rd-party Anthropic-compat proxy carries bogus signatures, and
   continuing under real Anthropic will fail signature validation on the
   next API request. There is no mechanism to detect or repair this.

The fix is to lift "conversation" out of any one profile, decompose it into
an ordered sequence of profile-pinned **segments**, treat a per-conversation
**central anatta-owned JSONL store** as the canonical source of truth, and
render an appropriate working view into the per-profile working area before
every backend spawn — applying family-aware sanitization when the target
family is stricter than a source segment's family.

This spec defines the architecture for the **claude backend, tier 1**.

## Scope and tiers

| Tier | Backend | Profile swap | Notes |
|---|---|---|---|
| **1** (this spec) | claude | within-claude (account swap, provider swap) | Anthropic ↔ 3rd-party compat; multi-account; multi-provider compat |
| 1.x | claude | sub-agent transcript sanitization | sanitize sidecar `subagents/*.jsonl` too |
| 2 | codex | within-codex | same architecture, different path conventions |
| 3 | claude ↔ codex | cross-backend | full transcoding; deferred |

Tier 1 does **not** introduce a new CLI subcommand for swap. The existing
`/profile` slash command in `apps/anatta-cli/src/chat/slash.rs` already
exposes the swap surface; tier 1 makes its behavior family-aware. Outside
chat, `anatta send --resume` participates via the same render/absorb
infrastructure.

## Non-goals

- No cross-backend swap (claude ↔ codex). Type-system invariant rejects
  segments whose profile backend differs from the conversation backend.
- No parsed-event mirror table in SQLite. JSONL on disk remains the only
  persistent form of conversation content. `AgentEvent` is in-memory only
  and is **never** serialized to disk as persistent state. The comment in
  `crates/anatta-core/src/agent_event.rs` ("Projection is one-way only:
  raw → unified") is reinforced into a hard architectural invariant.
- No new `chat swap` / `chat fork` subcommands. The existing `/profile`
  slash and the implicit profile arg on `chat new` are the only entry
  points.
- No content rewriting except by the StripReasoning sanitizer's local
  parent-pointer relink. In particular **no sessionId rewriting** — the
  conversation owns a single `session_uuid` (the one claude generated at
  the first turn) and every JSONL line under that conversation, in central
  or working storage, has that value.
- No automatic LLM-driven summarization. Compact is a transition hook that
  calls the CLI's native `/compact`; we never write our own summarization
  prompt.
- No sub-agent transcript sanitization (tier 1.x).
- No `Compact`, `Drop`, `ToolsOnly` *render-time* policies in tier 1.
  Only `Verbatim` and `StripReasoning` are implemented at render time.
  Compact is implemented as a separate `compact_before_close` transition
  hook. The other policy variants exist in the type system as placeholders
  for future tiers; the schema does not require migration to add them.
- No materialize-symlinks command. The design uses the existing shared
  `projects/` symlink only as a working area; the central store is the
  conversation's home.
- No server-side sync, cross-machine continuity, or multi-host lock.
- No retroactive change to `profile.id` format (stays as
  `<backend>-<8 char nanoid>`).
- No retroactive rename of conversation primary keys. `name` is currently
  the `conversations` PK; tier 1 adds an `id` (ULID) column and makes it
  the application-level primary key going forward, but keeps `name` as
  the DB PK and a `UNIQUE` index until a follow-up migration.

## Architectural inversion: central source of truth

The canonical JSONL for each segment of each conversation lives in an
anatta-owned central directory. The per-profile `projects/` (which today
is a symlink to a shared store) is treated as **derived working state**:
rendered fresh from central before each backend spawn, absorbed back
incrementally during the session, and **deleted at session end / profile
swap**.

```
┌─ Central source of truth (anatta owns) ───────────────────────────────┐
│  <anatta_home>/conversations/<conv-ulid>/                              │
│  └── segments/<segment-ulid>/                                          │
│      ├── events.jsonl           ← raw claude JSONL events for segment  │
│      └── sidecar/                                                      │
│          ├── subagents/         ← Task tool transcripts                │
│          └── tool-results/      ← large tool output offloaded by CLI   │
└────────────────────────────────────────────────────────────────────────┘
                            │ ↑
                  render    │ │  absorb
                  (per      │ │  (offset-based,
                  segment)  ▼ │   per turn)
┌─ Working area (CLI reads/writes; today symlinked, see "Existing       │
│   symlink architecture") ──────────────────────────────────────────────┐
│  <profile-dir>/projects/<encoded-cwd>/<session_uuid>.jsonl             │
│  <profile-dir>/projects/<encoded-cwd>/<session_uuid>/sidecar...        │
│  (Lifecycle: rendered at session start; multiple turns append to it;  │
│   deleted at session end / profile swap.)                              │
└────────────────────────────────────────────────────────────────────────┘
```

### Why this inversion is right

1. **CLI writes JSONL anyway** — we can't stop `claude` / `codex` from
   writing their session files. They are the upstream writer. The only
   question is who owns the canonical copy.
2. **Profile becomes credentialed-env-only** — under this architecture,
   `profile_dir/projects/` is scratch space. A profile is a `(credentials
   + env vars + family)` bundle. Profile deletion never loses conversation
   data.
3. **Cross-family sanitization fits naturally** — sanitization is a
   render-time transformation, not a separate "preprocess before swap"
   step. Each session renders the right view for *its* target profile.
4. **Server sync becomes a non-problem later** — central directory is the
   snapshot to push.
5. **Going back to a stricter family is safe** — if user goes lax → strict
   → lax → strict again, each render produces the right view from central
   anew; the working file's prior contents are irrelevant.

### Existing symlink architecture (not retired in tier 1)

`crates/anatta-runtime/src/profile/claude.rs:33-46` currently lays out
each claude profile with `projects/` as a symlink to
`<anatta_home>/shared/claude-projects/`. Tier 1 **keeps this symlink**:

- The shared dir is now treated as ephemeral working area, not as truth.
- For same-family swap, the shared file happens to already contain the
  right content (because both profiles produced compatible content into
  it). Re-rendering overwrites it idempotently — no harm done.
- For cross-family swap, render overwrites the shared file with the
  appropriate sanitized view for the target profile. Going back to the
  original family later re-renders from central, restoring the
  appropriate view.
- The shared file's contents are **never** read back as truth. Truth is
  always central.

Retiring the symlink and making each profile own a real `projects/`
directory is a possible future cleanup but out of tier 1 scope. The
present design is robust to the symlink either staying or going.

## Data model

### Existing tables (relevant context)

```sql
profile(
    id           TEXT PRIMARY KEY,         -- '<backend>-<8 char nanoid>'
    backend      TEXT NOT NULL,             -- 'claude' | 'codex'
    path         TEXT NOT NULL,             -- CLAUDE_CONFIG_DIR / CODEX_HOME
    provider     TEXT NOT NULL,             -- 'anthropic' | 'deepseek' | 'kimi' | 'openai' | 'custom' | ...
    base_url_override            TEXT,
    model_override               TEXT,
    small_fast_model_override    TEXT,
    default_opus_model_override  TEXT,
    default_sonnet_model_override TEXT,
    default_haiku_model_override TEXT,
    subagent_model_override      TEXT
);

conversations(
    name               TEXT NOT NULL PRIMARY KEY,
    profile_id         TEXT NOT NULL REFERENCES profile(id) ON DELETE RESTRICT,
    backend_session_id TEXT,
    cwd                TEXT NOT NULL,
    last_used_at       TEXT NOT NULL
);
-- NOTE: lock_holder_pid / lock_holder_started_at columns were dropped
-- in migration 0005. Locking is now SessionLock (flock under
-- <anatta_home>/runtime-locks/).
```

### New: `profile.family_override`

```sql
ALTER TABLE profile ADD COLUMN family_override TEXT;
-- NULL = derive from (backend, provider) using default_family()
-- non-NULL values: 'a-native' | 'a-compat' | 'o-native' | 'o-compat'
-- App-level validation on write.
```

Default derivation:

```rust
fn default_family(backend: Backend, provider: &str) -> Family {
    match (backend, provider) {
        (Claude, "anthropic") => Family::ANative,
        (Claude, _)           => Family::ACompat,   // safe-by-default: assume lax
        (Codex,  "openai")    => Family::ONative,
        (Codex,  _)           => Family::OCompat,
    }
}
```

**Safe-by-default rationale**: misclassifying lax as native causes the
target API to reject invalid signatures (hard failure). Misclassifying
native as lax only causes unnecessary stripping (soft loss of thinking
content). Default to lax; require explicit opt-in for native.

**Footgun**: `provider = "anthropic"` with `base_url_override` set may
actually be a 3rd-party proxy. Default classification still says
`ANative`; **a warning must be emitted at profile-create time** when
both fields are populated, suggesting `--family-override a-compat` if
the endpoint doesn't validate signatures (e.g., LiteLLM that doesn't
proxy thinking).

### Reshape: `conversations` (expand, not contract)

```sql
ALTER TABLE conversations ADD COLUMN id           TEXT;       -- ULID; backfilled in Rust
ALTER TABLE conversations ADD COLUMN backend      TEXT;       -- 'claude' | 'codex', from profile at creation time
ALTER TABLE conversations ADD COLUMN session_uuid TEXT;       -- UUID v4 from CLI; NULL until first turn done
ALTER TABLE conversations ADD COLUMN created_at   TEXT;       -- ISO-8601
```

The application treats `id` as the logical primary key (segments FK to
`conversations(id)`), but **the DB's PRIMARY KEY remains `name`** for
tier 1 to avoid a destructive `CREATE TABLE ... AS SELECT` rebuild. A
follow-up migration 0007 changes PK to `id` and adds `UNIQUE(name)`,
once all code paths have switched.

`backend_session_id` is the legacy column. Tier 1 uses the new
`session_uuid` column instead; legacy is kept for backward compat by
`anatta send --resume`'s reverse-lookup helper
(`crates/anatta-store/src/conversation.rs:94`). The legacy reverse-lookup
also gains a fallback to `session_uuid` so new conversations are found.

### New: `conversation_segments`

```sql
CREATE TABLE conversation_segments(
    id                  TEXT PRIMARY KEY,          -- ULID; minted in Rust
    conversation_id     TEXT NOT NULL,             -- references conversations(id) at app level
                                                   -- (no SQL FK until conversations PK becomes id in 0007)
    ordinal             INTEGER NOT NULL,          -- 0,1,2,...
    profile_id          TEXT NOT NULL REFERENCES profile(id) ON DELETE RESTRICT,
    source_family       TEXT NOT NULL,             -- frozen snapshot at creation:
                                                   -- 'a-native' | 'a-compat' | 'o-native' | 'o-compat'
    started_at          TEXT NOT NULL,
    ended_at            TEXT,                      -- NULL = active segment
    transition_policy   TEXT NOT NULL DEFAULT '{"kind":"verbatim"}',
                                                   -- JSON-encoded SegmentRenderPolicy that this segment
                                                   -- was *opened with* (the policy applied to prior segments
                                                   -- when their content was rendered into this segment's
                                                   -- first session's working file). Historical metadata.
    ended_with_compact  INTEGER NOT NULL DEFAULT 0,
                                                   -- 1 if /compact was fired before closing this segment
    last_absorbed_bytes INTEGER NOT NULL DEFAULT 0,
                                                   -- byte offset into the working file (across this
                                                   -- segment's lifetime) up to which content has been
                                                   -- absorbed to central. RESET to post-render size
                                                   -- after each render; ADVANCED to current size after
                                                   -- each absorb.
    UNIQUE (conversation_id, ordinal)
);

CREATE UNIQUE INDEX conversation_segments_one_active
    ON conversation_segments (conversation_id)
    WHERE ended_at IS NULL;
```

### Invariants (app-level)

1. `segment.profile.backend == conversation.backend` — segments cannot
   cross backend boundaries within one conversation.
2. At most one segment per conversation has `ended_at IS NULL` (enforced
   at DB level by the partial unique index).
3. `ordinal` is dense and monotonic per conversation (0, 1, 2, ...).
4. `source_family` is **frozen at segment creation** based on `family_of(profile)`
   at that moment. If the user later edits the profile's `family_override`,
   the historical segment's `source_family` does not change.
5. `conversation.session_uuid` is NULL **only before the first turn has
   produced any absorbed content**. Once the first turn completes
   successfully and at least one event has been absorbed, `session_uuid`
   is populated and never NULL again for that conversation's lifetime.
   See the first-turn flow under "Lifecycle integration" for the exact
   sequencing.
6. `synthesized_from` is intentionally absent. Render is stateless: it
   produces the working file from the ordered list of prior segments
   each time, applying per-segment policy. There is no segment-to-segment
   "I was synthesized from X" pointer.
7. `transition_policy` stores a JSON-encoded `SegmentRenderPolicy`. The
   serde tag is `kind`; the same representation is used in code and in
   the column.

### IDs

- `conversation.id` / `segment.id` → **ULID** (`ulid` crate, 26 chars,
  Crockford base32, time-sortable). Filesystem listings via `ls` show
  creation order. SQL `ORDER BY id DESC` is equivalent to `ORDER BY
  created_at DESC`. Visually distinguishable from claude's UUIDs in logs.
- `conversation.session_uuid` → **UUID v4** (assigned by claude, never
  by anatta). Anatta does not mint session UUIDs. The CLI generates one
  on the first turn; anatta captures it from the stream output (the
  existing first-event extraction in `spawn/mod.rs:223`) and persists.
- `profile.id` → existing format (`<backend>-<8 char nanoid>`), unchanged.

### ULID backfill is done in Rust, not SQL

SQLite cannot produce ULIDs natively. Migration 0006 adds the new columns
and table but leaves them empty. A one-time application bootstrap routine
runs at startup if any legacy conversation rows lack an `id`:

```rust
async fn backfill_ulids(store: &Store) -> Result<()> {
    for row in store.legacy_conversations_missing_id().await? {
        let conv_id = ulid::Ulid::new().to_string();
        let segment_id = ulid::Ulid::new().to_string();
        store.backfill_one(&row.name, &conv_id, &segment_id).await?;
    }
    Ok(())
}
```

The backfill also creates one segment-row per legacy conversation
(ordinal=0, profile_id = conversation.profile_id, source_family =
family_of(profile_at_backfill_time), last_absorbed_bytes = 0).

## Family classification

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    ANative,    // anthropic-native (Anthropic direct, any Anthropic account)
    ACompat,    // anthropic-compat (3rd-party proxies: deepseek, kimi, minimax, ...)
    ONative,    // openai-native (codex direct)
    OCompat,    // openai-compat (codex via 3rd-party endpoints; mostly hypothetical now)
}

impl Family {
    /// Higher = stricter. Strict validates signatures / encrypted state;
    /// lax does not. The asymmetry: lax→strict requires sanitization,
    /// strict→lax is verbatim.
    pub fn strictness(self) -> u8 {
        match self {
            Family::ACompat | Family::OCompat => 0,
            Family::ANative | Family::ONative => 1,
        }
    }

    pub fn parse(s: &str) -> Option<Self> { /* "a-native"/"a-compat"/"o-native"/"o-compat" */ }
    pub fn as_str(self) -> &'static str { /* round-trip */ }
}

pub fn family_of_profile(p: &Profile) -> Family {
    if let Some(o) = p.family_override.as_deref() {
        Family::parse(o).expect("validated at write time")
    } else {
        default_family(p.backend, &p.provider)
    }
}

pub fn needs_sanitize(src: Family, dst: Family) -> bool {
    dst.strictness() > src.strictness()
}
```

## Policy framework

### Render policy enum

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SegmentRenderPolicy {
    /// Pass through verbatim. Default for same-family or strict→lax transitions.
    Verbatim,

    /// Drop thinking-only assistant events; relink the DAG. Required when
    /// transitioning lax → strict.
    StripReasoning,

    // Reserved for future tiers. Schema accepts; runtime in tier 1 rejects
    // these at render time with a clear error.
    Compact { summary: CompactSummary },
    Drop,
    ToolsOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactSummary {
    Cached(String),
    LazyByTargetModel,
    LazyByProfile { profile_id: String },
}
```

**Note on the (segment, target_family) cache key** that was discussed mid-design:
the `cached_summary_target` field was considered and **rejected**. Summaries
generated by the CLI's `/compact` are plain text and family-agnostic; the
target model can ingest them regardless of whose tone they were written in.
The `CompactSummary` enum has no per-target dimension.

### Tier 1 implements only `Verbatim` and `StripReasoning`

Other variants exist as type-level placeholders. The framework can be
extended in later tiers without a schema migration (the column is `TEXT`
and accepts any JSON-stringified policy).

### Direction-aware minimum policy

```rust
pub fn min_policy_for(src: Family, dst: Family) -> SegmentRenderPolicy {
    if dst.strictness() > src.strictness() {
        SegmentRenderPolicy::StripReasoning
    } else {
        SegmentRenderPolicy::Verbatim
    }
}
```

This is the **lower bound**. Users may opt UP (more aggressive sanitization,
e.g., `Compact` or `Drop` for visual cleanliness) but cannot opt DOWN below
this minimum. **Tier 1 does not expose user-side opt-up at the CLI**; the
runtime always uses the minimum. Future tiers add a flag to `/profile` or
to `anatta chat new --policy`.

### The full compatibility matrix (claude tier 1)

| src \ dst | a-native | a-compat |
|---|---|---|
| **a-native** | Verbatim | Verbatim |
| **a-compat** | **StripReasoning** | Verbatim |

`o-native` and `o-compat` are symmetric for codex; codex implementation
deferred to tier 2.

## Transition hooks

Compact is **not** a render-time policy in tier 1. It is a hook fired on
the **outgoing** side of a swap, while the source CLI is still alive.

```rust
pub struct SegmentTransitionHook {
    /// If true, before closing the current segment, send /compact to the
    /// source profile (via `claude --print --resume <session_uuid> "/compact"`).
    /// The compact produces a compact_boundary event + a synthesized
    /// isCompactSummary user message; both are absorbed into the closing
    /// segment's central events.jsonl. `ended_with_compact` is then set to 1.
    pub compact_before_close: bool,

    /// What render policy the new segment uses to ingest prior segments.
    /// Defaults to min_policy_for(src.family, dst.family).
    pub next_segment_policy: SegmentRenderPolicy,
}
```

The compact subprocess is a **separate** `claude --print --resume <session_uuid>
"/compact"` invocation, spawned by anatta independently of the chat loop's
backend session. It does not require the chat loop's live CLI to remain
running; rather, anatta closes the chat-loop's CLI cleanly first, then
spawns the compact subprocess, then absorbs its output, then opens the
new segment.

**Tier 1**: `compact_before_close = false` by default. Users may set it
to `true` via a future CLI flag (out of scope here).

## The `/compact` mechanism (validated by spike 2026-05-11)

Empirically verified by spike (see `/private/tmp/anatta-compact-test/`):
`claude --print --resume <id> "/compact"` works and:

- Writes 1 `system/compact_boundary` event with `compactMetadata`
  (`trigger="manual"`, pre/post tokens, durationMs).
- Writes 1 synthesized `user` event with `isCompactSummary: true` carrying
  a markdown summary (`<previous-conversation-summary>` style).
- The compact_boundary has `parentUuid: null` + `logicalParentUuid` pointing
  back to the last pre-compact turn — claude's standard DAG break + soft-link
  pattern.
- The summary message has `parentUuid` pointing at the compact_boundary's
  uuid, re-establishing the DAG forward.
- stdout is empty (`num_turns: 0`); cost is non-zero (the API call happens).
- `entrypoint: "sdk-cli"` distinguishes anatta-triggered compacts from
  user-driven interactive compacts (`"cli"`).
- Post-compact session is fully resumable; subsequent turns read the
  summary as their conversation context.

**Implication for anatta**: we detect completion by waiting for the process
to exit and re-reading the JSONL, not by parsing stdout.

## DAG model (clarification)

A claude session JSONL is, in the common case, a **linear thread**:
events form a chain via `parentUuid` pointers. But the file's full data
model is a **DAG** (directed acyclic graph), and there are three distinct
ways non-linearity appears:

1. **Multiple roots**: events with `parentUuid: null`. A fresh session
   begins with an `attachment` event (root #1). Each `/compact` produces
   a `system/compact_boundary` event with `parentUuid: null` and a
   `logicalParentUuid` pointing back into the chain — i.e., a *new root*
   that soft-links to its logical predecessor.

2. **Forks from parallel tool calls**: when claude issues multiple
   `tool_use` blocks in one turn, the second `tool_use` chains its
   `parentUuid` to the first `tool_use`'s event uuid, and so does the
   first tool's `tool_result`. The first tool's event thus has **two
   children**: the next tool call and its own result. Same logical "turn",
   two-way fork in the DAG.

3. **Sidechains**: when claude invokes the `Task` tool, the sub-agent
   transcript is in a separate file (`<session_uuid>/subagents/agent-*.jsonl`),
   and the main session's events have `isSidechain: false` while sub-agent
   events have `isSidechain: true`. These are effectively disjoint DAGs.

For tier 1's sanitizer (StripReasoning):

- We **only sanitize the main JSONL**, not sub-agent transcripts (tier 1.x
  extends this).
- We **only drop thinking-only assistant events**, never compact boundaries,
  attachments, or tool_use/result events.
- We **only relink parentUuid locally** (one child of the dropped event gets
  its parentUuid rewired to the dropped event's parent). We do **not**
  touch `logicalParentUuid`.

Empirically verified across 5 real session files (270 + 180 + 7 + ... thinking-only
events) that thinking-only events always have exactly 1 child and are never
adjacent to another thinking-only event — the invariant the sanitizer relies on.

## Render: central → working

### Path computation

```rust
/// Working-file path for the given conversation under the given profile.
fn working_jsonl_path(profile: &Profile, conv: &Conversation) -> PathBuf {
    let session_uuid = conv.session_uuid
        .as_ref()
        .expect("caller must check session_uuid is populated before render");
    profile.path
        .join("projects")
        .join(encode_cwd(&conv.cwd))
        .join(format!("{session_uuid}.jsonl"))
}

fn working_sidecar_dir(profile: &Profile, conv: &Conversation) -> PathBuf {
    let session_uuid = conv.session_uuid.as_ref().unwrap();
    profile.path
        .join("projects")
        .join(encode_cwd(&conv.cwd))
        .join(session_uuid.to_string())
}

/// cwd encoding: replace '/' with '-'. The cwd is already canonicalized
/// at conversation creation time, so no re-canonicalize here.
fn encode_cwd(canonical_cwd: &str) -> String {
    canonical_cwd.replace('/', "-")
}
```

### Cwd canonicalization

Conversation creation (`apps/anatta-cli/src/chat/mod.rs:42`,
`apps/anatta-cli/src/chat/runner.rs:49`) currently stores `cwd` as
supplied. Tier 1 changes this to `std::fs::canonicalize(supplied_cwd)?`
at insert time. macOS `/tmp` resolves to `/private/tmp`; claude itself
canonicalizes internally, so we must match.

Conversation cwd is *immutable* after creation. If the directory is later
deleted or moved, render fails at `std::fs::create_dir_all` with a clear
error. There is no "rebind cwd" command in tier 1.

### Render algorithm

```rust
pub async fn render(
    conv: &Conversation,
    profile: &Profile,
    store: &Store,
) -> Result<RenderOutcome, RenderError> {
    // First-turn case: no session_uuid yet → no render (CLI generates).
    let Some(session_uuid) = conv.session_uuid.as_ref() else {
        return Ok(RenderOutcome::SkippedFirstTurn);
    };

    let segments = store.load_segments(&conv.id).await?;
    let target_family = family_of_profile(profile);
    let working = working_jsonl_path(profile, conv);
    std::fs::create_dir_all(working.parent().unwrap())?;

    // Atomic write: .tmp then rename
    let tmp = working.with_extension("jsonl.tmp");
    {
        let mut out = BufWriter::new(File::create(&tmp)?);
        for seg in &segments {
            let src_family = Family::parse(&seg.source_family)
                .expect("validated at write time");
            let policy = min_policy_for(src_family, target_family);
            let src_path = store.segment_events_path(&seg);
            if !src_path.exists() {
                // First segment may legitimately have no events yet if the
                // first turn never completed. Skip.
                continue;
            }
            let src_file = File::open(&src_path)?;
            match policy {
                SegmentRenderPolicy::Verbatim => {
                    let mut r = BufReader::new(src_file);
                    std::io::copy(&mut r, &mut out)?;
                }
                SegmentRenderPolicy::StripReasoning => {
                    sanitize::strip_reasoning(BufReader::new(src_file), &mut out)?;
                }
                other => {
                    return Err(RenderError::PolicyNotImplemented(other));
                }
            }
        }
        out.flush()?;
    }

    // Mirror sidecar from central → working. All-or-nothing: write to a temp
    // sibling directory and rename, or rollback on partial failure.
    let working_sidecar = working_sidecar_dir(profile, conv);
    let tmp_sidecar = working_sidecar.with_extension("sidecar.tmp");
    let sidecar_result = (|| -> Result<bool, std::io::Error> {
        let mut any = false;
        for seg in &segments {
            let src = store.segment_sidecar_dir(&seg);
            if src.exists() {
                copy_dir_recursive(&src, &tmp_sidecar)?;
                any = true;
            }
        }
        Ok(any)
    })();
    match sidecar_result {
        Ok(true) => {
            if working_sidecar.exists() {
                std::fs::remove_dir_all(&working_sidecar)?;
            }
            std::fs::rename(&tmp_sidecar, &working_sidecar)?;
        }
        Ok(false) => { /* no sidecar to mirror */ }
        Err(e) => {
            // Clean up tmp directory; do not affect main file.
            let _ = std::fs::remove_dir_all(&tmp_sidecar);
            return Err(e.into());
        }
    }

    // Commit main file
    std::fs::rename(&tmp, &working)?;

    // The active segment's last_absorbed_bytes is now the size of the
    // working file at this moment (everything in it came from central).
    let size = std::fs::metadata(&working)?.len();
    store.set_segment_offset(&active_segment.id, size).await?;

    Ok(RenderOutcome::Rendered { bytes: size })
}

pub enum RenderOutcome {
    SkippedFirstTurn,
    Rendered { bytes: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("policy not implemented in tier 1: {0:?}")]
    PolicyNotImplemented(SegmentRenderPolicy),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sanitize(#[from] sanitize::SanitizeError),
    #[error(transparent)]
    Store(#[from] anatta_store::StoreError),
}
```

### Key properties

- **No sessionId rewriting** anywhere — sanitizer preserves all fields it
  doesn't touch, and copy_with_policy::Verbatim is literally a byte copy.
- **Atomic main-file write** via `.tmp` + `rename`. Claude must never
  observe a half-written file.
- **All-or-nothing sidecar copy** via tmp directory + rename, with rollback
  on partial failure.
- **last_absorbed_bytes reset** after render to current size: everything
  in the file at this moment came from central, so absorb tracks only the
  bytes added beyond this point.

## Absorb: working → central

### Trigger

After each CLI exit within a session. Working file is **not** deleted; only
the offset is advanced. Crash-recovery absorb on anatta startup uses the
same code path.

### Algorithm (offset-based, "A1")

```rust
pub async fn absorb(
    seg: &Segment,
    conv: &Conversation,
    profile: &Profile,
    store: &Store,
) -> Result<(), AbsorbError> {
    let working = working_jsonl_path(profile, conv);
    if !working.exists() {
        return Ok(()); // nothing to absorb (race with cleanup)
    }
    let cur_size = std::fs::metadata(&working)?.len();
    if cur_size < seg.last_absorbed_bytes {
        // Anomalous: working file shrunk. Abort and report.
        return Err(AbsorbError::WorkingFileShrunk {
            segment_id: seg.id.clone(),
            previous: seg.last_absorbed_bytes,
            current: cur_size,
        });
    }
    if cur_size == seg.last_absorbed_bytes {
        return Ok(()); // nothing new
    }

    let central = store.segment_events_path(seg);
    std::fs::create_dir_all(central.parent().unwrap())?;

    // Copy [last_absorbed_bytes .. cur_size) to central.
    // Crash-idempotency: we re-derive offset from central file size on
    // re-entry, so duplicate appends would be caught (see below).
    let mut src = File::open(&working)?;
    src.seek(SeekFrom::Start(seg.last_absorbed_bytes))?;
    let n = (cur_size - seg.last_absorbed_bytes) as usize;
    let mut buf = vec![0u8; n];
    src.read_exact(&mut buf)?;

    // Idempotency: compare central file's current size against expected.
    // If central is shorter than (last_absorbed_bytes - bytes_in_segment_at_render),
    // we crashed mid-absorb. Reseek the working file to match central's actual
    // position before re-appending. For tier 1 simplicity we use a simpler
    // approach: write to a tmp file, append, then atomic rename.
    let tmp_central = central.with_extension("jsonl.tmp");
    if central.exists() {
        std::fs::copy(&central, &tmp_central)?;
    }
    {
        let mut dst = OpenOptions::new().append(true).create(true).open(&tmp_central)?;
        dst.write_all(&buf)?;
        dst.flush()?;
    }
    std::fs::rename(&tmp_central, &central)?;

    // Sidecar mirror: copy any new files from working sidecar to central sidecar.
    let work_sidecar = working_sidecar_dir(profile, conv);
    if work_sidecar.exists() {
        sync_sidecar(&work_sidecar, &store.segment_sidecar_dir(seg))?;
    }

    store.set_segment_offset(&seg.id, cur_size).await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AbsorbError {
    #[error("working file shrunk for segment {segment_id} ({previous} → {current} bytes)")]
    WorkingFileShrunk { segment_id: String, previous: u64, current: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] anatta_store::StoreError),
}
```

### Crash idempotency

If anatta crashes after the central tmp-rename but before
`set_segment_offset`, on next startup we attempt absorb again with the
old `last_absorbed_bytes`. The same bytes would be appended a second
time, corrupting central. To prevent this:

- Use the **tmp-rename** pattern shown above, **plus**:
- On crash recovery, before absorbing, compare central file's actual
  byte size against `last_absorbed_bytes`:
  - If `central_size + (working_size - last_absorbed_bytes) == working_size`
    (the absorb didn't happen), proceed.
  - If `central_size >= working_size` after subtracting the
    pre-segment offset (the absorb did happen but offset wasn't
    updated), skip the append and just call `set_segment_offset` to
    catch up.

Simpler invariant: the rule is **`central_size at end = working_size`
for any session that's done absorbing**. On recovery:

```
expected_central = working_size - render_initial_size
actual_central   = file_size(central)
if actual_central == expected_central → absorb already done; just update offset
if actual_central <  expected_central → resume from actual_central
if actual_central >  expected_central → corruption; abort with error
```

Where `render_initial_size` is `last_absorbed_bytes` *at the time render
completed*. Tier 1 stores this explicitly:

```sql
ALTER TABLE conversation_segments ADD COLUMN render_initial_bytes INTEGER NOT NULL DEFAULT 0;
```

Render sets both `last_absorbed_bytes` and `render_initial_bytes` to the
post-render size. Absorb advances only `last_absorbed_bytes`. Crash
recovery uses `render_initial_bytes` to compute expected central size.

### Sidecar mirror

```rust
fn sync_sidecar(src: &Path, dst: &Path) -> std::io::Result<()> {
    // For each file in src that isn't in dst (or differs in size), copy it.
    // For each subdirectory, recurse.
    // No deletions: sidecar files are append-only from the CLI's perspective.
    // ... (implementation detail)
}
```

**Sidecar filename collision policy**: claude generates sidecar filenames
internally (e.g., `<random>.txt` for tool-results, `agent-<id>.jsonl` for
subagents). Collisions between absorbs of different turns are not expected.
If they ever occur (anomalous), `sync_sidecar` errors out with the
conflicting path; the user surfaces this as a corrupted-state warning.
Tier 1 does not attempt to merge or auto-rename.

## Working area lifecycle

| Event | Action |
|---|---|
| **Conversation create (first turn ever)** | session_uuid is NULL → spawn CLI without `--session-id`. CLI generates the UUID, writes to working file (no prior render). |
| **First turn completes** | Capture session_uuid from stream; UPDATE conversations. Run absorb → segment 0's central events.jsonl. Update last_absorbed_bytes to working file size. |
| **Subsequent turn in same segment, working file exists** | Skip render. Spawn CLI with `--resume <session_uuid>`. |
| **Subsequent turn in same segment, working file missing** | Render from central. Spawn CLI with `--resume <session_uuid>`. |
| **After each CLI exit** | Absorb new bytes; update offset. **Do not delete** working file. |
| **Session end** (`anatta chat` exits cleanly) | Final absorb. Delete working file + sidecar dir. Reset `last_absorbed_bytes` to 0. |
| **Profile swap** (in-chat via `/profile`) | See "Profile swap flow" below. |
| **Anatta crash** | On next startup, before opening lock, scan for active-segment working files; run absorb in crash-recovery mode (see "Crash idempotency"). Then proceed normally. |

The user explicitly chose this lifecycle: deletion is at **coarse** events
(session end / profile switch), not per-turn. Multiple turns reuse the
same working file. Re-render is paid only when starting a session fresh
or switching profiles.

### Empty first-turn (user exits before sending anything)

The user starts `anatta chat new <name> --profile <p>`, the runner inserts
the conversation row and segment 0 row, acquires the lock, prompts the
user — and the user immediately Ctrl-Ds.

In this state:
- `conversation.session_uuid` is NULL
- segment 0 exists with `last_absorbed_bytes = 0` and no events.jsonl in
  central
- no working file was ever rendered (because session_uuid was NULL)

On next `anatta chat resume <name>`:
- session_uuid is still NULL → first-turn path runs again (CLI generates
  UUID; render is skipped).

The empty first-turn case is **idempotent**: until the user actually sends
a prompt, the conversation has no committed content and can be re-entered
as if fresh.

## Lifecycle integration

### First-turn flow (new conversation)

```
1. User: anatta chat new <name> --profile <p>
2. Acquire SessionLock(name)
3. canonical_cwd = canonicalize(supplied_cwd)
4. INSERT conversations
     id=ulid(), name, cwd=canonical_cwd, backend=p.backend,
     session_uuid=NULL, created_at=now, last_used_at=now
5. INSERT segment 0
     id=ulid(), conversation_id=conv.id, ordinal=0,
     profile_id=p.id, source_family=family_of(p),
     started_at=now, transition_policy='{"kind":"verbatim"}',
     last_absorbed_bytes=0, render_initial_bytes=0
6. Spawn CLI without --session-id (no render; let CLI generate)
7. Wait for first stream event; extract session_id field
8. UPDATE conversations SET session_uuid=<extracted> WHERE id=conv.id
9. Chat loop:
     For each turn:
        CLI runs, appends to working file
        On CLI exit: absorb()
10. Session ends:
     Final absorb()
     Delete working file + sidecar
     UPDATE segments SET last_absorbed_bytes=0, render_initial_bytes=0
     Release lock
```

### Resume flow (no profile change)

```
1. User: anatta chat resume <name>
2. Acquire SessionLock(name)
3. Load conversation + active segment (call it active_seg)
4. profile = open profile by active_seg.profile_id
5. Render (idempotent; skipped if session_uuid is NULL)
6. Spawn CLI with --resume <session_uuid> (if non-NULL) else without
7. Chat loop as above
8. Session end as above
```

### Profile-swap flow (in-chat `/profile`)

`apps/anatta-cli/src/chat/slash.rs::handle_profile` currently picks a
different profile and calls `runner::session.swap(new_launch)`. Tier 1
extends this:

```
1. User invokes /profile in chat
2. Picker shows profiles. User picks new profile P_new
3. Reject cross-backend (existing check). Continue if same-backend.
4. Acquire active_seg from store; sanity-check
5. (Optional, future) If transition_policy_override.compact_before_close:
     a. End the current chat-loop CLI session (it's per-turn anyway in claude)
     b. Spawn `claude --print --resume <session_uuid> "/compact"`
        with CLAUDE_CONFIG_DIR = old profile's path
     c. Wait for exit; absorb any new lines into active_seg
     d. Set ended_with_compact = 1
6. Final absorb of active_seg under old profile
7. Delete old profile's working file + sidecar
8. UPDATE active_seg SET ended_at = now
9. INSERT new_seg
     ordinal=active_seg.ordinal + 1,
     profile_id=P_new.id,
     source_family=family_of(P_new),
     transition_policy=min_policy_for(active_seg.source_family, family_of(P_new))
                         (as JSON),
     last_absorbed_bytes=0, render_initial_bytes=0
10. Render under P_new (renders ALL prior segments[0..=active_seg], applying
     per-segment policy min_policy_for(seg.source_family, family_of(P_new)))
11. Set new_seg.last_absorbed_bytes = render_size
                  .render_initial_bytes = render_size
12. Update session.swap to use the new launch (existing mechanism)
13. Continue chat loop under new profile
```

#### Swap rollback semantics

If any step 5–11 fails, the database state must be rolled back:
- If step 8 succeeded but step 9 failed: rollback `ended_at` on
  active_seg (set back to NULL). Caller surfaces error.
- If step 9 succeeded but step 10 failed: delete new_seg row, rollback
  active_seg.ended_at. Caller surfaces error.
- If step 10 partially wrote files: render's RAII guard cleans up the
  tmp main file and tmp sidecar dir; working location is unchanged.

Implementation: wrap the DB updates in a single SQLite transaction
(steps 8+9+11), and only commit when render (step 10) returns Ok.
File-system operations in step 7 (delete old working) happen *after*
the DB transaction commits, so a failed render leaves both DB and
filesystem consistent with "swap didn't happen".

### Same-family swap = same code path

Same-family swap (e.g., Anthropic account A → account B) flows through
the exact same code path above; step 9's `min_policy_for` returns
`Verbatim` for every prior segment, render is byte-copy throughout.

**Why same-family still pays for a render copy**: the user pushed back
on "if same UUID, why copy?" The answer: because profile B's CLI must
find a file at `<B>/projects/<encoded_cwd>/<session_uuid>.jsonl`, and
without copying we have only the file at `<A>/projects/...` — which is
the same physical inode (thanks to shared `projects/` symlink) only on
*existing* profile pairs. Render performs a defensive copy that:

- Costs ~ O(few MB) in practice (negligible).
- Becomes structurally important when `projects/` symlinks are eventually
  retired (post-tier-1).
- Makes the "central is canonical" invariant hold regardless of symlink
  state.

The user's pushback is partially honored: in tier 1, since `projects/` is
shared by symlink, the same-family render's overwrite happens to write
identical bytes to where the file already lived. We do not optimize this
away — keeping a single code path (always render) is simpler than a
conditional "skip render if symlink target is already the file".

### Crash recovery on startup

```
On anatta startup, before the first CLI command runs:
   For each conversation with an active segment (ended_at IS NULL):
      working = working_jsonl_path(...)
      if working exists:
         attempt SessionLock(conv.name) — if already locked by another
             live process, skip
         absorb() in crash-recovery mode (uses render_initial_bytes
             to validate)
         release lock
```

This is a best-effort sweep. On any error, log + continue (don't block
the user from running `anatta chat`).

### `anatta send --resume` integration

`apps/anatta-cli/src/send.rs:79+` resolves a backend session id, looks up
the conversation (by `backend_session_id` or new `session_uuid`), acquires
the SessionLock, spawns the backend.

Tier 1 changes `send --resume`:
1. Look up conversation by `session_uuid` (fall back to legacy
   `backend_session_id` for older rows).
2. Active segment determines the profile (active_seg.profile_id).
3. Same render + spawn + absorb cycle as chat.
4. Single-turn semantics: render → spawn one turn → absorb → release lock.

There is no profile-swap surface in `send` (use chat for swap).

## Path encoding rules (claude)

Empirically verified:

```
canonical absolute path:    /Users/wangxiuxuan/Developer/anatta
encoded for projects/:      -Users-wangxiuxuan-Developer-anatta

canonical absolute path:    /private/tmp/anatta-compact-test
                            (macOS canonicalizes /tmp → /private/tmp)
encoded for projects/:      -private-tmp-anatta-compact-test
```

`conversations.cwd` stores the canonical form (post-`std::fs::canonicalize`).
All path derivations use it verbatim.

## Field rewriting summary

| Field | Render | Absorb | Sanitizer (StripReasoning) |
|---|---|---|---|
| `sessionId` | unchanged | unchanged | unchanged |
| `uuid` | unchanged | unchanged | dropped events removed; kept events preserve uuid |
| `parentUuid` | unchanged | unchanged | rewritten only if it pointed at a dropped uuid (→ grandparent) |
| `logicalParentUuid` | unchanged | unchanged | unchanged |
| `cwd` | unchanged | unchanged | unchanged |
| `gitBranch` / `version` / `entrypoint` / `slug` / `timestamp` | unchanged | unchanged | unchanged |
| `message.content[*]` | unchanged | unchanged | thinking-only events: whole event dropped |
| `tool_use.input` / `tool_result.content` | unchanged | unchanged | unchanged |
| any other field | unchanged | unchanged | unchanged |

## StripReasoning sanitizer

### Parse via `serde_json::Value`, NOT typed `ClaudeEvent`

`crates/anatta-runtime/src/claude/history.rs` defines strict tagged enums
that **fail to round-trip unknown fields** (the `serde` derive doesn't
preserve extras). Using these for sanitization would silently lose any
field claude added in a version anatta hasn't been updated for.

Sanitizer therefore parses each line as `serde_json::Value`, inspects the
shape it needs (`type`, `message.content`, `uuid`, `parentUuid`), and
re-emits with `serde_json::to_string`. Unknown fields preserved verbatim.

### Algorithm

```rust
pub fn strip_reasoning<R: BufRead, W: Write>(src: R, mut dst: W) -> Result<(), SanitizeError> {
    // 1. Parse all lines into Value, collecting (uuid, parentUuid, is_thinking_only)
    let mut lines: Vec<(Value, Option<String>, Option<String>, bool)> = Vec::new();
    for line in src.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let v: Value = serde_json::from_str(&line).map_err(|e| SanitizeError::Parse {
            line: line.clone(), source: e,
        })?;
        let uuid = v.get("uuid").and_then(|x| x.as_str()).map(String::from);
        let parent = v.get("parentUuid").and_then(|x| x.as_str()).map(String::from);
        let is_to = is_thinking_only_assistant(&v);
        lines.push((v, uuid, parent, is_to));
    }

    // 2. Build maps: dropped uuids → their parent
    let mut drop_uuids: HashSet<String> = HashSet::new();
    let mut new_parent: HashMap<String, Option<String>> = HashMap::new();

    // Defensive: check invariants (single child, no chained thinking)
    for (i, (_v, uuid, _parent, is_to)) in lines.iter().enumerate() {
        if !is_to { continue; }
        let uuid = uuid.as_deref().unwrap_or("?");
        let children: Vec<_> = lines.iter()
            .filter(|(_v, _u, p, _)| p.as_deref() == Some(uuid))
            .collect();
        if children.len() != 1 {
            // Fallback: keep the event but blank out the thinking text.
            // (Out of scope for tier-1 simple algorithm; we error to surface.)
            tracing::warn!(uuid = uuid, n_children = children.len(),
                "StripReasoning: thinking event has unexpected child count; keeping with blanked thinking");
            // Tier 1: blank the thinking content in-place.
            blank_thinking_in_place(&mut lines[i].0);
            continue;
        }
        let parent_uuid = &lines[i].2;
        if let Some(p) = parent_uuid {
            if lines.iter().any(|(_v, u, _p, is_to2)| {
                u.as_deref() == Some(p) && *is_to2
            }) {
                tracing::warn!(uuid = uuid, "StripReasoning: parent is also thinking-only; blanking");
                blank_thinking_in_place(&mut lines[i].0);
                continue;
            }
        }
        drop_uuids.insert(uuid.to_string());
        new_parent.insert(uuid.to_string(), parent_uuid.clone());
    }

    // 3. Emit non-dropped events, rewriting parentUuid if it pointed at a dropped uuid
    for (v, uuid, _parent, _is_to) in &lines {
        if let Some(u) = uuid {
            if drop_uuids.contains(u) { continue; }
        }
        let mut v = v.clone();
        if let Some(p) = v.get("parentUuid").and_then(|x| x.as_str()) {
            if let Some(grandparent) = new_parent.get(p) {
                v.as_object_mut().unwrap().insert(
                    "parentUuid".into(),
                    grandparent.clone().map(Value::String).unwrap_or(Value::Null),
                );
            }
        }
        writeln!(dst, "{}", serde_json::to_string(&v)?)?;
    }
    Ok(())
}

fn is_thinking_only_assistant(v: &Value) -> bool {
    v.get("type").and_then(|x| x.as_str()) == Some("assistant")
        && v.get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|blocks| !blocks.is_empty() && blocks.iter().all(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("thinking")
            }))
            .unwrap_or(false)
}

fn blank_thinking_in_place(v: &mut Value) { /* sets each thinking block's text to "" */ }
```

### Behavior on parse failure

Sanitizer **fails loudly** on a malformed JSONL line, returning
`SanitizeError::Parse { line, source }`. It does NOT silently drop bad
lines. The render call propagates this error; the user sees a clear
"corrupted history" message and can intervene.

## Sidecar handling

### Discovered structure (empirically validated)

```
<encoded_cwd>/
├── <session_uuid>.jsonl
├── <session_uuid>/              ← only present when sub-agents / large tool outputs used
│   ├── subagents/
│   │   ├── agent-<id>.jsonl
│   │   └── agent-<id>.meta.json
│   └── tool-results/
│       └── <random>.txt
└── memory/                      ← project-level (per-cwd, NOT per-session)
    └── *.md
```

### Tier 1 handling

| Item | Render | Absorb |
|---|---|---|
| Main JSONL | per-segment policy | offset-based append |
| `<session_uuid>/subagents/` | all-or-nothing copy (tmp + rename) | mirror new files |
| `<session_uuid>/tool-results/` | all-or-nothing copy | mirror new files |
| `memory/` | not touched (this is per-cwd shared, lives at `<encoded_cwd>/memory/` outside any session uuid) | not touched |

**Sub-agent transcripts** are not sanitized in tier 1 (their thinking
blocks remain). Tier 1.x extends the sanitizer to walk these files too.

## Migration 0006: schema changes (expand-only)

```sql
-- crates/anatta-store/migrations/0006_segments.sql

BEGIN;

-- 1. profile.family_override
ALTER TABLE profile ADD COLUMN family_override TEXT;

-- 2. conversations new columns
ALTER TABLE conversations ADD COLUMN id            TEXT;
ALTER TABLE conversations ADD COLUMN backend       TEXT;
ALTER TABLE conversations ADD COLUMN session_uuid  TEXT;
ALTER TABLE conversations ADD COLUMN created_at    TEXT;

-- 3. conversation_segments
CREATE TABLE conversation_segments(
    id                  TEXT PRIMARY KEY,
    conversation_id     TEXT NOT NULL,
    ordinal             INTEGER NOT NULL,
    profile_id          TEXT NOT NULL REFERENCES profile(id) ON DELETE RESTRICT,
    source_family       TEXT NOT NULL,
    started_at          TEXT NOT NULL,
    ended_at            TEXT,
    transition_policy   TEXT NOT NULL DEFAULT '{"kind":"verbatim"}',
    ended_with_compact  INTEGER NOT NULL DEFAULT 0,
    last_absorbed_bytes INTEGER NOT NULL DEFAULT 0,
    render_initial_bytes INTEGER NOT NULL DEFAULT 0,
    UNIQUE (conversation_id, ordinal)
);

CREATE UNIQUE INDEX conversation_segments_one_active
    ON conversation_segments (conversation_id)
    WHERE ended_at IS NULL;

COMMIT;
```

### Application-side backfill (run once at startup after migration)

```rust
async fn backfill_for_0006(store: &Store) -> Result<()> {
    let legacy_rows = store.conversations_missing_new_columns().await?;
    for row in legacy_rows {
        let conv_id = ulid::Ulid::new().to_string();
        let seg_id = ulid::Ulid::new().to_string();
        let backend = store.profile_backend(&row.profile_id).await?;
        let source_family = derived_family(backend, &row.provider, &row.family_override).as_str();
        store.backfill_conversation_and_segment(BackfillArgs {
            name: &row.name,
            conv_id: &conv_id,
            backend: backend.as_str(),
            session_uuid: row.backend_session_id.as_deref(),
            created_at: &row.last_used_at,
            seg_id: &seg_id,
            profile_id: &row.profile_id,
            source_family,
            started_at: &row.last_used_at,
            // central events.jsonl may not exist if no first turn happened yet
            render_initial_bytes: 0,
            last_absorbed_bytes: 0,
        }).await?;

        // If the legacy session has an existing JSONL in the shared projects dir,
        // copy it into central segment 0 events.jsonl.
        if let Some(uuid) = row.backend_session_id.as_deref() {
            let legacy_path = legacy_working_path(&row.cwd, uuid);
            if legacy_path.exists() {
                let central_path = central_events_path(&conv_id, &seg_id);
                std::fs::create_dir_all(central_path.parent().unwrap())?;
                std::fs::copy(&legacy_path, &central_path)?;
                let size = std::fs::metadata(&central_path)?.len();
                store.update_segment_offsets(&seg_id, size, size).await?;

                // Also mirror sidecar dir if present
                let legacy_sidecar = legacy_path.with_extension("");
                if legacy_sidecar.is_dir() {
                    let central_sidecar = central_path.parent().unwrap().join("sidecar");
                    copy_dir_recursive(&legacy_sidecar, &central_sidecar)?;
                }
            }
        }
    }
    Ok(())
}
```

The backfill is idempotent: it re-runs on every startup but only processes
rows that don't yet have `id` populated.

### Migration 0007 (deferred follow-up, NOT this migration)

After all code paths use the new tables, a follow-up migration:
- Adds `NOT NULL` to `conversations.id`, `.backend`, `.created_at`
- Adds `UNIQUE` on `conversations.id`
- Drops `conversations.profile_id` and `conversations.backend_session_id`
- Changes PRIMARY KEY from `name` to `id` (SQLite recreate-table dance)

Out of tier 1 scope.

## Central directory layout

```
<anatta_home>/                                    (default: ~/.anatta)
├── anatta.db                                     ← existing SQLite
├── shared/claude-projects/                       ← existing shared symlink target;
│                                                   used as working area in tier 1
├── runtime-locks/                                ← existing flock-based locks
├── profiles/<backend>-<shortid>/                 ← existing
│   ├── projects/  → ../../shared/claude-projects (existing symlink)
│   └── ... (settings, credentials)
└── conversations/<conv-ulid>/                    ← NEW
    └── segments/<segment-ulid>/                  ← NEW
        ├── events.jsonl
        └── sidecar/
            ├── subagents/
            └── tool-results/
```

`anatta_home` is resolved by `apps/anatta-cli/src/config.rs:resolve()`:
`$ANATTA_HOME` env var, else `~/.anatta`.

`Store` is extended with path-helper methods:

```rust
impl Store {
    pub fn anatta_home(&self) -> &Path { &self.anatta_home }
    pub fn conversations_root(&self) -> PathBuf { self.anatta_home.join("conversations") }
    pub fn conv_dir(&self, conv_id: &str) -> PathBuf { self.conversations_root().join(conv_id) }
    pub fn segment_dir(&self, conv_id: &str, seg_id: &str) -> PathBuf {
        self.conv_dir(conv_id).join("segments").join(seg_id)
    }
    pub fn segment_events_path(&self, conv_id: &str, seg_id: &str) -> PathBuf {
        self.segment_dir(conv_id, seg_id).join("events.jsonl")
    }
    pub fn segment_sidecar_dir(&self, conv_id: &str, seg_id: &str) -> PathBuf {
        self.segment_dir(conv_id, seg_id).join("sidecar")
    }
}
```

Today `Store` only holds a `SqlitePool` (see `crates/anatta-store/src/lib.rs:26`).
Tier 1 expands `Store::open` to also accept `anatta_home: PathBuf` and
store it.

## What stays the same

- `profile.path` (CLAUDE_CONFIG_DIR), credential file mechanism (file-based
  per recent refactor `f2e8414`), OAuth flow.
- `crates/anatta-runtime/src/claude/{history,stream,projector}.rs` —
  parser + projection logic. Sanitizer is a new consumer of the *raw*
  format (via `serde_json::Value`), not of the typed parser.
- `AgentEvent` and projection contract — in-memory only, never persisted.
- `crates/anatta-runtime/src/session_lock.rs` — flock-based per-conversation
  lock; locked on conversation `name`.
- The existing shared `projects/` symlink architecture in
  `crates/anatta-runtime/src/profile/claude.rs`.

## What gets deleted

Nothing is deleted in tier 1. Migration 0007 (future) drops legacy
`conversations.profile_id` and `conversations.backend_session_id` once
all callers cut over.

## Tier 1 implementation checklist

| # | Item | Files (likely) |
|---|---|---|
| M1 | Add `ulid = "1"` to `[workspace.dependencies]`; consume in store/runtime | `Cargo.toml`, `crates/anatta-store/Cargo.toml` |
| M2 | Migration 0006 | `crates/anatta-store/migrations/0006_segments.sql` |
| M3 | Application-side ULID backfill for legacy conversations | `crates/anatta-store/src/conversation.rs`, `apps/anatta-cli/src/main.rs` bootstrap |
| M4 | `Family` enum + `family_of_profile()` + classification tests | new `crates/anatta-runtime/src/profile/family.rs` |
| M5 | `SegmentRenderPolicy` enum + serde + `min_policy_for()` | new `crates/anatta-runtime/src/profile/policy.rs` |
| M6 | `profile.family_override` column accessor + CLI flag on profile create | `crates/anatta-store/src/profile.rs`, `apps/anatta-cli/src/profile.rs` |
| M7 | Profile-create-time warning: `provider=anthropic` + `base_url_override` | `apps/anatta-cli/src/profile.rs` |
| M8 | `conversation_segments` CRUD | new `crates/anatta-store/src/segment.rs` |
| M9 | `conversations` CRUD updates (id, session_uuid, backend, created_at accessors) | `crates/anatta-store/src/conversation.rs` |
| M10 | `Store` central-path helpers + anatta_home threading | `crates/anatta-store/src/{lib.rs,paths.rs}` |
| M11 | Cwd canonicalization on conversation create | `apps/anatta-cli/src/chat/mod.rs::run_new` |
| M12 | StripReasoning sanitizer (Value-based, defensive fallback) | new `crates/anatta-runtime/src/claude/sanitize.rs` |
| M13 | Render function | new `crates/anatta-runtime/src/conversation/render.rs` |
| M14 | Absorb function (with crash-idempotent semantics) | new `crates/anatta-runtime/src/conversation/absorb.rs` |
| M15 | Sidecar helpers (copy-dir-recursive, sync-sidecar) | new `crates/anatta-runtime/src/conversation/sidecar.rs` |
| M16 | First-turn session_uuid capture and persistence | `apps/anatta-cli/src/chat/runner.rs`, possibly `spawn/claude.rs` |
| M17 | Wire render/absorb into chat loop (per-turn absorb, session-end cleanup) | `apps/anatta-cli/src/chat/runner.rs` |
| M18 | Wire render/absorb into `anatta send --resume` | `apps/anatta-cli/src/send.rs` |
| M19 | Extend `/profile` slash-command swap with family-aware new-segment creation + render | `apps/anatta-cli/src/chat/slash.rs`, `apps/anatta-cli/src/chat/runner.rs` |
| M20 | Crash-recovery absorb sweep at anatta startup | `apps/anatta-cli/src/main.rs` (bootstrap) |
| M21 | Legacy JSONL → central segment-0 backfill at startup (idempotent) | as part of M3 |
| M22 | Tests | unit tests in each new module; integration tests in `crates/anatta-runtime/tests/` |

## Tests

### Unit tests

- `Family::strictness()` ordering, `parse`/`as_str` round-trip.
- `family_of_profile()` defaults across (backend, provider) combinations,
  with and without `family_override`.
- `needs_sanitize()` matrix.
- `min_policy_for()` returns StripReasoning iff dst strictly stricter.
- `encode_cwd()` against known inputs.
- `strip_reasoning()` against:
  - synthetic single-thinking-block fixture (drop + relink)
  - thinking event with 0 children → fallback path (blank in place)
  - thinking event with >1 children → fallback path
  - chained thinking → fallback for both
  - mixed-content event (text + thinking) → out of empirical range; tier 1
    asserts and uses fallback (blank in place)
  - real fixture extracted from `~/.claude/projects/-Users-wangxiuxuan-Developer-anatta/d387dd1a-...jsonl` (3295 events, 270 thinking-only)
  - real fixture extracted from `~/.claude/projects/-Users-wangxiuxuan-Developer-anatta/fac447f6-...jsonl` (1742 events, 180 thinking-only)
- `render()` + `absorb()` round-trip on synthetic fixtures (Verbatim, StripReasoning).
- Crash-recovery absorb: simulate partial central write, verify idempotency via `render_initial_bytes`.

### Integration tests (require live `claude` binary; behind feature flag)

- E2E first turn: `anatta chat new foo --profile <claude-anthropic>`,
  first turn produces a session_uuid; central segment 0 contains absorbed events.
- E2E same-profile resume: existing conversation, restart anatta, render
  re-creates working file, claude resumes successfully.
- E2E same-family swap: account A → account B; new segment opens; renders
  prior segment under new profile; claude continues conversation.
- E2E cross-family swap (a-compat → a-native): DeepSeek profile → Anthropic
  profile; StripReasoning applied; thinking events absent in new working
  file; claude continues without API rejection. (Requires a DeepSeek-style
  profile; can be simulated with a mock proxy if needed.)
- E2E `send --resume`: existing conversation, `anatta send --resume <id>`
  works the same as before but now backed by render/absorb.
- Crash recovery: kill anatta mid-session; on restart, absorb recovers
  unabsorbed turns; idempotent re-run does not duplicate.

## Open questions deferred to future tiers

- **User-facing policy opt-up surface** — flag on `/profile`, `chat new`,
  or `send --resume`. Tier 1 always uses `min_policy_for`.
- **`anatta profile materialize`** — relevant only if `projects/` symlink
  is retired. Defer until then.
- **`anatta conv export` / `import`** — central directory is portable;
  small wrapper later.
- **Sub-agent transcript sanitization** — tier 1.x.
- **Codex parity** — tier 2; same architecture, different path
  conventions, different /compact mechanism (codex JSON-RPC, not slash command).
- **Cross-backend swap** — tier 3.
- **Conversation rename** — straightforward once `id` is PK (segments FK
  by id, name is just a UNIQUE column). Surface via `anatta chat rename`
  not in tier 1.
- **Retire `projects/` symlink** — orthogonal to tier 1; can ship as a
  later cleanup once central store has been the truth for a release cycle.

## Validation references

- `/compact via --print` empirically verified: see test session
  `/private/tmp/anatta-compact-test/` and the resulting JSONL at
  `~/.claude/projects/-private-tmp-anatta-compact-test/2fe68faa-086e-4b3e-8f01-f762e45748ef.jsonl`.
  Before /compact: 14 lines; after /compact: 25 lines including 1
  `compact_boundary` + 1 `isCompactSummary: true` user message.
  Token reduction observed: 31441 → 2660 (~12×).
- Thinking-event DAG invariant verified across 5 sessions in
  `~/.claude/projects/-Users-wangxiuxuan-Developer-anatta/`:
  fac447f6 (180 thinking-only, 0 forks, 0 chained, 0 leaf),
  f7983672 (7 thinking-only, 0 forks, 0 chained, 0 leaf),
  d387dd1a (270 thinking-only, 0 forks, 0 chained, 0 leaf).
- Path encoding verified empirically: `/tmp` → `/private/tmp` on macOS,
  then `/` → `-`.
- `--session-id <uuid>` flag accepts user-supplied UUIDs (claude `--help`
  documents this; tier 1 does not use it as a primary mechanism, only as
  a defensive option if needed).
- Existing shared `projects/` symlink: confirmed at
  `crates/anatta-runtime/src/profile/claude.rs:33-46`.
- Existing `SessionLock` (flock-based, lives in `<anatta_home>/runtime-locks/`):
  confirmed at `crates/anatta-runtime/src/session_lock.rs:35`.
- DB lock columns dropped: confirmed at
  `crates/anatta-store/migrations/0005_drop_lock_columns.sql`.
- Existing `/profile` slash and `Session::swap`: confirmed at
  `apps/anatta-cli/src/chat/slash.rs` and
  `crates/anatta-runtime/src/spawn/session.rs:203-209`.
- Config root `~/.anatta`: confirmed at
  `apps/anatta-cli/src/config.rs:24`.
- `nanoid` for profile short-ids: confirmed at
  `crates/anatta-runtime/src/profile/mod.rs:40-55`.

## Codex audit reconciliation

This spec incorporates findings from codex audit round 1 (2026-05-12).
Key changes made:

- Removed erroneous reference to PID-based lock columns (4.8).
- Corrected config root to `~/.anatta` (4.9).
- Added existing `/profile` slash and `Session::swap` integration (4.4, 4.5).
- Added `family_override` CLI flag (4.6).
- Added cwd canonicalization (4.7).
- Documented existing shared `projects/` symlink (4.1, 4.2).
- Specified `transition_policy` storage format as JSON-encoded TEXT (1.2).
- Specified `session_uuid` invariant timing (1.3).
- Documented `synthesized_from` absence as intentional (1.5).
- Sanitizer uses `serde_json::Value` to preserve unknown fields (4.11, 1.6).
- Added crash-idempotency via `render_initial_bytes` column (2.1).
- Added crash-recovery absorb sweep with lock acquisition (2.2).
- Added swap rollback semantics (2.3).
- Added empty first-turn handling (2.4).
- Resolved first-turn chicken-and-egg by skipping render until session_uuid known (2.5).
- Fixed shrunk-file pseudocode (2.6).
- Added `anatta send --resume` integration (2.7).
- Documented sidecar collision policy: error out (2.8, 2.9).
- ULID backfill done in Rust (3.2).
- Cwd existence check at render time, error if missing (3.3).
- File size cap explicitly not specified in tier 1 (3.4).
- `Store` extended with central-path helpers (3.5).
- Lock identity is conversation `name` (stable; the SessionLock is keyed
  on opaque string; renaming conversations is not in tier 1) (3.6).
- Same-family render copy justified (codex JSONL c1).
- `cached_summary_target` rejection documented (codex JSONL c2).
- DAG model section added (codex JSONL b2).
- "All-or-nothing" copy made explicit in render/sanitizer/sidecar (codex JSONL b3).
- Source family snapshot frozen at segment creation; profile name not
  snapshotted but profile_id FK preserves provenance via segment lifetime
  (codex JSONL a).
