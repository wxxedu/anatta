# `anatta` cross-engine swap — tier 3 design

**Status**: design draft, pending codex review + spike verification
**Date**: 2026-05-13
**Owner**: wxx
**Builds on**: `2026-05-12-conversation-segments-design.md` (tier 1/2)

## Problem

Tier 1/2 lets a user swap *profile* mid-conversation as long as the new profile
shares the same backend (claude ↔ claude or codex ↔ codex). The
`/profile` slash command in `apps/anatta-cli/src/chat/slash.rs` and the
runtime's `Session::swap` both reject cross-backend swap explicitly:

```rust
// crates/anatta-runtime/src/spawn/session.rs:134
(s, l) => Err(SwapError::BackendMismatch {
    current: s.kind(),
    target: l.kind(),
}),
```

The tier 1/2 spec lists cross-engine swap as **Tier 3, deferred**, and
records the invariant that makes it impossible today:

> Invariant 1: `segment.profile.backend == conversation.backend` — segments
> cannot cross backend boundaries within one conversation.

This spec replaces that invariant and lays out everything needed to swap
claude ↔ codex inside a single conversation: schema changes, on-disk
layout, the new transcoder module, render-flow changes, the codex
resume mechanism (with its empirical-verification gate), CLI surface
changes, and the migration that lifts existing conversations into the
new model.

## Scope

In-scope:

- Same-conversation cross-engine swap via `/profile` (claude ↔ codex).
- Full-fidelity transcoding for text + tool calls + tool results across
  engines, including recursive sub-agent transcripts.
- Lazy per-target cache of transcoded views at the central store.
- Cross-engine `anatta send --resume`.
- Migration of legacy single-backend conversations to the new
  segment-owns-backend model.

Out of scope:

- Cross-engine *reasoning* preservation. `thinking` (claude) and
  `reasoning` (codex) carry per-vendor signed / encrypted payloads that
  cannot be made valid for the other engine. They are dropped at the
  engine boundary. (Within a single engine, the existing
  `min_policy_for(src_family, dst_family)` continues to govern reasoning
  preservation.)
- Cross-engine sub-agent *thread continuation*. We faithfully transcode
  sub-agent transcripts so the new engine can read them, but anatta does
  not preserve the ability to "address the same sub-agent again" across
  engines — the sub-agent context is one-way replayable history.
- Live (`AgentEvent`-stream-based) cross-engine handoff. Tier 3 is
  segment-level swap (close source segment, open target segment), not
  intra-segment mid-turn handover.
- Sandbox / approval policy reconciliation. Codex carries `sandbox` and
  `approvalPolicy` per turn; claude has no analogue. Transcode emits a
  reasonable default in the codex `turn_context` and drops them when
  going to claude. Future work can let users set these per profile.

## Non-goals (architectural)

- No revival of "AgentEvent as persistent canonical format". `AgentEvent`
  remains an in-memory unified projection; the persistent canonical for
  each segment stays in that segment's *producing engine's* native wire
  format. Cross-engine transparency is achieved via on-demand
  transcoded *views* at the central store, not by abandoning native
  shapes.
- No automatic LLM-driven summarization at engine boundaries. The
  existing `/compact` transition hook (tier 1) is still optional; tier 3
  does not require it because text + tool calls + tool results are
  carried forward natively.
- No retroactive change to `profile.id` format, `conversations.id` format,
  or central store directory layout above the segment level.

## Architectural inversion vs tier 1/2

Tier 1/2 model:

```
conversation —(N:1)— backend
conversation —(1:N)— segments  (all segments share conversation.backend)
segment.events.jsonl = canonical, claude or codex native shape (decided by conversation.backend)
render = central → working, per-segment family-aware sanitization
```

Tier 3 model:

```
conversation has no backend
conversation —(1:N)— segments  (each segment carries its own backend)
segment.events.jsonl = canonical, source engine's native shape
segment.views/<engine>/* = cached transcoded views, on-demand
render = central → working; for cross-engine segments, route through transcoder cache, then apply family policy
```

Two orthogonal axes:

| Axis | Tier 1/2 mechanism | Tier 3 addition |
|---|---|---|
| Family (signature/encryption strictness, within one engine) | `min_policy_for(src_family, dst_family)` → Verbatim / StripReasoning | unchanged |
| Engine (wire format) | rejected at swap | transcoder route + view cache |

`min_policy_for` continues to govern within-engine. `transcode_to(target_engine, ...)` is a separate pass that runs *first* when src_engine ≠ dst_engine; its output then participates in the family-policy stage like any same-engine segment.

## Data model

### Schema changes

Tier 3 migration is `migration 0007_cross_engine.sql`. It is an
**expand-only** migration: existing columns are dropped via `DROP COLUMN`
only after a Rust-side backfill copies data into the new shape.

#### `conversations`

```sql
-- Backfill engine_session_id into the first segment.
-- (See "Migration" section for the Rust backfill routine.)

ALTER TABLE conversations DROP COLUMN backend;
ALTER TABLE conversations DROP COLUMN session_uuid;
-- Legacy backend_session_id retained (existing tier 1 compat for send --resume reverse-lookup).
```

After 0007, `conversations` carries only `id`, `name`, `cwd`,
`backend_session_id` (legacy), `created_at`, `last_used_at`.

#### `conversation_segments`

```sql
ALTER TABLE conversation_segments ADD COLUMN backend TEXT NOT NULL DEFAULT 'claude';
ALTER TABLE conversation_segments ADD COLUMN engine_session_id TEXT;
-- engine_session_id is NULL only before the segment's first turn has produced any absorbed content;
-- once populated, it never goes back to NULL.

-- The DEFAULT 'claude' lets the migration ALTER TABLE succeed; the Rust backfill
-- then sets the correct backend per row (derived from segment.profile_id → profile.backend).
```

After 0007, `conversation_segments` has all tier 1/2 columns plus
`backend` and `engine_session_id`.

### Invariants (replacing tier 1's Invariant 1)

1. **A segment carries one backend.** Mid-segment cross-engine handoff is
   not possible; cross-engine swap always opens a new segment.
2. **`(segment.backend, segment.engine_session_id)` is the segment's
   resume coordinate** in its source engine's namespace. For claude
   this is the claude `sessionId`; for codex this is the codex
   `threadId`.
3. **A conversation may contain any sequence of segment backends.**
   Adjacent segments may share a backend or differ.
4. **`segment.source_family` is frozen at segment creation** and remains
   meaningful even in tier 3: when rendering for a target profile in
   the same engine family, family-aware policy still applies.
5. **`segment.profile.backend == segment.backend`.** A segment is bound
   to one profile, whose backend determines the segment's backend.
6. **`conversation_segments_one_active` (one active segment per
   conversation)** continues to hold across engines.
7. **`engine_session_id` is opaque within DB.** It is interpreted by
   render and absorb code per the segment's `backend`.

### IDs

- `conversation.id`, `segment.id` → ULID (unchanged from tier 1).
- `segment.engine_session_id` → opaque string, populated by the engine
  on first turn. Claude generates UUID v4 (`019b...` shape via newer
  CLI); codex generates ULID-as-UUID (`019b8c2e-fe72-...`). Anatta does
  not mint either of these.
- `view_engine_session_id` → synthesized id used only inside transcoded
  views' working area. Computed deterministically (see §3.3). Never
  written to DB.

## Storage layout

### Central store (canonical + views)

```
<anatta_home>/conversations/<conv-ulid>/
└── segments/<segment-ulid>/
    ├── meta.json                       ← {backend, source_family, ...}; redundant with DB, self-describing
    ├── events.jsonl                    ← canonical, source engine's native wire shape
    ├── sidecar/                        ← canonical sidecar
    │   └── (claude: subagents/, tool-results/, ... )
    │     (codex:  sub-agent rollouts captured during absorb)
    └── views/                          ← lazy per-target transcoded cache
        ├── claude/                     ← only present when source_engine != "claude"
        │   ├── _meta.json              ← {transcoder_version, mtime_of_source_at_build, source_byte_hash}
        │   ├── events.jsonl
        │   └── sidecar/
        │       └── subagents/agent-*.jsonl
        └── codex/                      ← only present when source_engine != "codex"
            ├── _meta.json
            ├── rollout.jsonl
            └── subagents/<sub_thread_id>.jsonl
```

`_meta.json` carries the cache key:

```json
{
  "transcoder_version": 1,
  "source_canonical_size_at_build": 12345,
  "source_canonical_sha256_prefix": "ab12cd34",
  "view_engine_session_id": "019c..."
}
```

Render checks `transcoder_version == TRANSCODER_VERSION` and
`source_canonical_size_at_build == current_canonical_size`. Mismatch →
rebuild.

The `source_canonical_size` check is conservative: for an *ended*
segment, canonical does not grow. For an *active* segment, canonical
does grow (each turn appends). But active segment is always in the
current engine — we never need a cross-engine view of it. So the
size-stability check is correct.

### Working area (per-engine, per-segment)

The active segment's working area is the path the live engine reads /
writes during a turn.

```
claude target:
  <claude_profile.path>/projects/<encoded_cwd>/<active_segment.engine_session_id>.jsonl
  <claude_profile.path>/projects/<encoded_cwd>/<active_segment.engine_session_id>/sidecar/...

codex target:
  <codex_profile.path>/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<active_segment.engine_session_id>.jsonl
  <codex_profile.path>/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<sub_thread_id>.jsonl   (one per sub-agent)
  + possibly session_index.jsonl entry (spike-dependent; see §10)
  + possibly state_5.sqlite entry (spike-dependent; see §10)
```

For an active segment that is currently being chatted under its source
engine, the working file is built by:
1. For each prior segment, the policy-applied bytes (canonical OR view).
2. The "prior" portion of the current segment's own canonical (already
   absorbed turns).

No transcoded view is required for the active segment itself — we never
view a segment under an engine other than its source while it's still
the one being written to.

## Transcoder module

### Layout

```
crates/anatta-runtime/src/transcode/
├── mod.rs              ← public API + TranscodeError + TRANSCODER_VERSION
├── id_mint.rs          ← deterministic synthetic id helpers
├── claude_to_codex/
│   ├── mod.rs          ← main-line transcoder
│   ├── sub.rs          ← recursive sub-agent transcoder
│   └── tools.rs        ← tool name + id mapping
└── codex_to_claude/
    ├── mod.rs
    ├── sub.rs
    └── tools.rs
```

### Public API

```rust
pub const TRANSCODER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine { Claude, Codex }

pub struct TranscodeInput<'a> {
    pub source_engine: Engine,
    pub source_events_jsonl: &'a Path,
    pub source_sidecar_dir: &'a Path,         // may not exist
    pub source_engine_session_id: &'a str,    // for synthetic id derivation
    pub conversation_cwd: &'a str,            // codex preamble needs cwd
}

pub struct TranscodeOutput {
    pub view_engine_session_id: String,
    pub view_events_path: PathBuf,
    pub view_sidecar_dir: PathBuf,
}

pub fn transcode_to(
    target: Engine,
    input: TranscodeInput,
    view_dir: &Path,
) -> Result<TranscodeOutput, TranscodeError>;

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("source events.jsonl malformed at line {line}: {source}")]
    Parse { line: usize, source: serde_json::Error },
    #[error("missing sub-agent transcript: {path}")]
    MissingSubAgent { path: PathBuf },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported source variant: {0}")]
    Unsupported(String),
}
```

### ID minting (deterministic)

All synthetic ids the transcoder mints are deterministic functions of
the source. A given segment, transcoded into a given target engine,
produces the same view_engine_session_id every time.

```rust
// in id_mint.rs
use uuid::Uuid;

const NAMESPACE_ANATTA_VIEW: Uuid = Uuid::from_bytes([
    0x61, 0x6e, 0x61, 0x74, 0x74, 0x61, 0x76, 0x69,
    0x65, 0x77, 0x76, 0x31, 0x00, 0x00, 0x00, 0x00,
]); // ASCII 'anattaview' + 'v1' + padding

pub fn view_session_id(source_engine_session_id: &str, target: Engine) -> String {
    let key = format!("session::{}::{}", source_engine_session_id, target.as_str());
    Uuid::new_v5(&NAMESPACE_ANATTA_VIEW, key.as_bytes()).to_string()
}

pub fn view_sub_thread_id(
    parent_view_id: &str,
    sub_agent_index: usize,
) -> String {
    let key = format!("sub::{}::{}", parent_view_id, sub_agent_index);
    Uuid::new_v5(&NAMESPACE_ANATTA_VIEW, key.as_bytes()).to_string()
}

pub fn map_tool_call_id(source_id: &str, source_engine: Engine, target_engine: Engine) -> String {
    let prefix = match (source_engine, target_engine) {
        (Engine::Claude, Engine::Codex) => "anatta-cc-",
        (Engine::Codex, Engine::Claude) => "anatta-cx-",
        _ => return source_id.to_owned(),
    };
    format!("{prefix}{source_id}")
}

pub fn synth_claude_uuid(parent_view_id: &str, line_index: usize) -> String {
    let key = format!("uuid::{}::{:08}", parent_view_id, line_index);
    Uuid::new_v5(&NAMESPACE_ANATTA_VIEW, key.as_bytes()).to_string()
}
```

Deterministic ids matter because:

1. The cache view's working-area filename uses `view_engine_session_id`.
   If it changed across rebuilds, the codex `thread/resume <id>` issued
   to the live app-server would target a thread it can't find.
2. Sub-agent ids in the parent must match the sub rollout's
   `session_meta.id` — deterministic generation makes them match by
   construction.
3. Cache invalidation tests can assert the *content* of a rebuilt cache
   matches a prior known-good build byte-for-byte (where transcoding is
   deterministic anyway).

### claude → codex mapping (main line)

| Claude wire element | Codex emission |
|---|---|
| First-line `system/init` | `session_meta { id: view_id, originator: "anatta", cwd, cli_version: "anatta-transcoder", timestamp, model_provider: "openai" (placeholder) }` then `turn_context { cwd, model: "(transcoded)", approval_policy: "never", sandbox_policy: {type:"dangerFullAccess"} }` |
| `user` `message.content[]` `type:"text"` | `response_item::Message { role: "user", content: [InputText{text}] }` |
| `user` `message.content[]` `type:"image"` | `response_item::Message { role: "user", content: [InputImage{...}] }` |
| `user` `message.content[]` `type:"tool_result"` | `response_item::FunctionCallOutput { call_id: map_tool_call_id(tool_use_id, Claude, Codex), output: content_as_value }` |
| `user` event flagged `isCompactSummary: true` | inline `compacted { message: summary_text }` (precedes a fresh `turn_context` block to mirror codex's compact representation) |
| `assistant` `message.content[]` `type:"text"` | `response_item::Message { role: "assistant", content: [OutputText{text}] }` |
| `assistant` `message.content[]` `type:"thinking"` | **drop** |
| `assistant` `message.content[]` `type:"tool_use"` (name != "Task") | `response_item::FunctionCall { call_id: map_tool_call_id(tool_use_id, Claude, Codex), name, arguments: serde_json::to_string(input)? }` |
| `assistant` `message.content[]` `type:"tool_use"` name=="Task" | **Spike-pending (§10b).** Initial codex review of 434 real rollouts found 0 occurrences of `event_msg::collab_agent_spawn_end`; collab/sub-agent emission shape is unproven. Until the spike resolves this, transcoder represents Task as a plain `response_item::FunctionCall { name: "Task", arguments }` plus a paired `FunctionCallOutput` carrying the sub-agent's final assistant message; the recursive sub-rollout is still emitted but the new engine will see it as a labeled tool call, not as a tracked collab thread |
| `system/compact_boundary` event | suppressed (the synthesized `isCompactSummary` user that follows produces the `compacted` row) |
| `attachment` event | `response_item::Message { role: "user", content: [InputText{text: "[attachment: <type>: <path>]"}] }` (claude attachments don't have a clean codex equivalent; degrade to descriptive text) |
| Any `parentUuid`, `uuid`, `logicalParentUuid`, `sessionId`, `gitBranch`, `version`, `entrypoint`, `slug`, `timestamp`, `isSidechain` fields | dropped (codex has no DAG model and no equivalent envelope) |

DAG → linear flatten:

- Codex rollouts are linear, append-only. Claude DAGs are usually linear
  but may fork (parallel tool calls) or have multiple roots (compact
  boundaries).
- Transcoder traverses in `parentUuid`-order DFS, producing a linear
  sequence. Forks are emitted in declaration order (the order the
  tool_use blocks appeared in the assistant message).
- Multiple-root segments (post-compact resumes within one segment)
  emit the compact summary as a `compacted` line plus a re-initialized
  `turn_context` line, mirroring how codex represents its own
  in-segment compacts.

### codex → claude mapping (main line)

| Codex wire element | Claude emission |
|---|---|
| First-line `session_meta` | `system/init { sessionId: view_id, cwd: meta.cwd, version: "anatta-transcoder", entrypoint: "anatta", slug: "anatta-view", model: turn_context.model, gitBranch: meta.git.branch or "", ... }` |
| `turn_context` (subsequent or paired with session_meta) | dropped (claude has no equivalent line type; model info is forwarded into `system/init` only) |
| `response_item::Message{role:"user", content:[InputText]}` | `user` line with `message.content[].type="text"`, fresh uuid + parentUuid=prev_uuid |
| `response_item::Message{role:"user", content:[InputImage]}` | `user` line with `message.content[].type="image"` |
| `response_item::Message{role:"assistant", content:[OutputText]}` | `assistant` line with `message.content[].type="text"` |
| `response_item::Reasoning` | **drop** |
| `response_item::FunctionCall` | `assistant` line, `message.content[].type="tool_use"`, `{id: map_tool_call_id(call_id, Codex, Claude), name, input: json_decode(arguments)}` |
| `response_item::FunctionCallOutput` | `user` line, `message.content[].type="tool_result"`, `{tool_use_id: map_tool_call_id(call_id, Codex, Claude), content: stringify(output)}` |
| `response_item::CustomToolCall` / `CustomToolCallOutput` | mapped same as FunctionCall / FunctionCallOutput |
| `response_item::WebSearchCall` | mapped to a `tool_use{name:"WebSearch", input: action}` plus, if the next item is a paired `WebSearchEnd` in event_msg, a synthesized `tool_result` |
| `response_item::GhostSnapshot` | dropped (claude has no analogue) |
| `event_msg::AgentMessage` | dropped — the same content was already emitted via the paired `response_item::Message{assistant,OutputText}` |
| `event_msg::UserMessage` | dropped — same reason |
| `event_msg::CollabAgentSpawnEnd` (if encountered; **rare per spike data**) | `assistant` line `tool_use{name:"Task", input:{description: new_agent_role, prompt, subagent_type: new_agent_nickname}, id: map_tool_call_id(call_id, Codex, Claude)}`; recurse on the named `new_thread_id` sub-rollout located via state_5.sqlite (see §absorb-sub-agent) |
| `event_msg::CollabAgentInteractionEnd` (rare) | `user` line `tool_result{tool_use_id: map_tool_call_id(call_id), content: "(interaction follow-up: \"<prompt>\")"}` plus subsequent `tool_use{name:"Task", id: <fresh>}` for the follow-up turn |
| `event_msg::CollabCloseEnd` (rare) | dropped (claude has no Task-close event) |
| `event_msg::CollabWaitingEnd` (rare) | dropped |
| `response_item::FunctionCall { name == "spawn_agent" \| similar mcp name }` (**spike to discover**) | mapped to `tool_use{name:"Task", ...}` if spike reveals codex sub-agents are emitted as a function-call shape rather than the collab event_msg |
| `event_msg::ExecCommandEnd` / `PatchApplyEnd` / `McpToolCallEnd` | dropped — the paired `FunctionCallOutput` already carries the tool result; these event_msgs are codex's own UI-facing duplicates |
| `event_msg::TokenCount`, `TaskStarted`, `TaskComplete`, `TurnAborted` | dropped |
| `event_msg::GuardianAssessment` | dropped (no claude analogue) |
| `event_msg::ContextCompacted` | mapped to a `system/compact_boundary` + synthesized `isCompactSummary` user line, using surrounding context for the summary text |
| `event_msg::ThreadNameUpdated` | dropped |
| `event_msg::WebSearchEnd` | (handled with the preceding `WebSearchCall`) |
| `event_msg::ViewImageToolCall` | mapped to a `tool_use{name:"ViewImage", input:{path}}` + synthesized empty `tool_result` |
| `compacted` line | `system/compact_boundary` + synthesized `isCompactSummary` user with `message`'s text |
| Codex `call_id`, `turn_id`, `phase`, `namespace` envelope fields | dropped |

UUID synthesis (codex → claude):

- Walk codex lines in file order, line_index starts at 0.
- For each emitted claude line, `uuid = synth_claude_uuid(view_id, line_index)`.
- `parentUuid = uuid_of_previous_emitted_line`; first line `parentUuid = null`.
- `sessionId = view_id` on every line.
- `cwd = conversation_cwd`.

### Sub-agent recursion

#### claude → codex

When transcoder encounters `assistant.message.content[].tool_use.name == "Task"`:

1. `sub_use_id = the tool_use.id` (a `toolu_*` id).
2. Locate sub transcript by scanning `<source_sidecar_dir>/subagents/`
   for `agent-*.jsonl` + matching `agent-*.meta.json`. **Filenames are
   keyed by claude's `agentId` (e.g. `agent-af18b008bf0e68f93.jsonl`),
   NOT by `tool_use_id`.** The link from tool_use_id → agentId is:
   - The meta.json file contains `{agentType, description, ...}`.
   - The parent's `tool_use.input` carries `{subagent_type, description, prompt}`.
   - Match heuristic: pair sub-agent files to parent tool_uses by:
     1. Primary key: `(subagent_type == agentType) AND (description == description)`.
     2. Tiebreaker 1: `sha256(input.prompt)` prefix == hash of the
        sub-agent's first user message (claude's sub-agent starts with
        the parent's prompt verbatim).
     3. Tiebreaker 2: declaration order within the segment (left-to-right
        across parallel Task tool_uses) matched against agent file mtime.
     - If after all three an ambiguity persists, demote ALL the
       ambiguous parent Task calls to "unmatched" — better to show no
       transcript than to attach a wrong one.
3. If no matching sub-agent file found, emit a degraded `tool_result`
   with text "(sub-agent transcript unavailable)" rather than failing
   the entire transcode.
4. Compute `sub_view_id = view_sub_thread_id(view_id, sub_index)`.
5. Recurse: `transcode_to(Engine::Codex, TranscodeInput { source_events_jsonl: sub_path, source_engine_session_id: <claude agentId>, ... }, view_dir.join("subagents").join(format!("{sub_view_id}.jsonl")))`.
6. **Spike-pending shape** (aligned with §3.4): emit the Task spawn as
   a plain `response_item::FunctionCall { call_id, name: "Task",
   arguments: json_encode(input) }` in the parent codex view, paired
   with a `response_item::FunctionCallOutput { call_id, output:
   <sub-agent final assistant message text> }` carrying the sub's
   summary. The recursive sub rollout file is still produced (so
   future tiers / a spike-confirmed `CollabAgentSpawnEnd` path can
   adopt it), but the parent view does **not** reference it via
   thread_id until the spike resolves how codex represents tracked
   sub-agent spawning on disk.
7. (When the spike unlocks the collab form, an additional emission
   step becomes: emit `event_msg::CollabAgentSpawnEnd { ...,
   new_thread_id: sub_view_id }` and write any thread_spawn_edges DB
   side-effect needed. Out of scope for tier 3 v1.)

#### codex → claude

The codex source-side sub-agent spawn shape is **spike-pending**
(§3.4 + §10). The transcoder supports two source shapes:

**Shape (i)**: a `response_item::FunctionCall { name == "Task" }` —
the v1 fallback. In this case the parent rollout itself encodes the
spawn as a plain function call; the sub-agent's transcript (if any)
is stored under sidecar/subagents/ keyed by the call_id. Mapping:
1. Locate sub rollout at `<source_sidecar_dir>/subagents/<call_id>.jsonl`
   (claude→codex transcoder wrote it there in §3.7 step 5; native codex
   transcripts don't currently produce this — see Shape (ii)).
2. If file missing, emit parent `tool_use{name:"Task"}` and a
   placeholder `tool_result{content:"(sub-agent transcript unavailable)"}`
   (don't fail the whole transcode).
3. Recurse if found, mark sub lines `isSidechain: true`.

**Shape (ii)**: an `event_msg::CollabAgentSpawnEnd { new_thread_id }` —
the post-spike shape, when codex actually emits this form. In this case:
1. Look up the sub rollout via state_5.sqlite (`thread_spawn_edges`
   joined to `threads.rollout_path`); absorb has already copied it to
   `<source_sidecar_dir>/subagents/<new_thread_id>.jsonl`.
2. If file missing, `TranscodeError::MissingSubAgent` (this should
   not happen post-absorb; raising loudly catches absorb bugs).
3. Compute `sub_view_id = view_sub_thread_id(view_id, sub_index)`.
4. Recurse: transcode the codex sub rollout into a claude sidechain
   jsonl at `view_dir.join("sidecar/<view_id>/subagents/").join(format!("agent-{sub_view_id}.jsonl"))`. Mark every line with `isSidechain: true`.

Both shapes converge on emitting:
- Parent `tool_use{name:"Task", input:{prompt, subagent_type, description}, id: <mapped call_id>}`
- Paired `tool_result{tool_use_id: <same>, content: <sub-agent final agent_message>}`

### Codex sub-agent absorb (new step)

Codex sub-agents are independent sessions. Their existence and rollout
path are tracked in **`<CODEX_HOME>/state_5.sqlite`**. Round-2 spike
finding: schema is

- `threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, ...)` —
  every codex thread, including sub-agent threads. **Column is `id`, not
  `thread_id`.**
- `thread_spawn_edges(parent_thread_id TEXT, child_thread_id TEXT,
  status TEXT, ...)` — parent→child relation.

Exact column lists are recorded in spike notes once observed.

Tier 1 absorb only handles the main rollout. Tier 3 absorb extends:

```
After absorbing main rollout for a codex segment:
  1. Open <CODEX_HOME>/state_5.sqlite via sqlx with:
       SqliteConnectOptions::new()
           .filename("<CODEX_HOME>/state_5.sqlite")
           .read_only(true)
           .busy_timeout(Duration::from_secs(3))
  2. SELECT child_thread_id FROM thread_spawn_edges
       WHERE parent_thread_id = <segment.engine_session_id>.
  3. For each child:
       SELECT rollout_path FROM threads WHERE id = child.
       Copy that rollout file → <central_segment_dir>/sidecar/subagents/<child_thread_id>.jsonl.
  4. If any child has further descendants (depth > 1), recurse.
```

The codex_to_claude transcoder later reads this sidecar dir.

**Failure mode**: if `state_5.sqlite` cannot be opened (e.g., schema
mismatch, lock timeout exceeded), log a warning naming the segment and
skip sub-agent absorb for that codex segment. The main rollout still
absorbs normally; sub-agents simply won't appear in later cross-engine
renders. A future tier can add a retry-on-next-startup mechanism.

### Atomicity

```
transcode_to writes:
  <view_dir>.tmp/                      (created if not present)
    _meta.json
    events.jsonl  (or rollout.jsonl)
    sidecar/...
Then atomically renames <view_dir>.tmp/ → <view_dir>/.
On any error during write, removes the .tmp dir; <view_dir>/ untouched.
```

Sub-agent transcode recurses with its own atomic tmp dir nested under
the parent's tmp dir.

### Versioning + invalidation

`TRANSCODER_VERSION` is bumped any time:

- A mapping rule changes.
- An id-minting algorithm changes.
- A new event variant is added that the old transcoder would have dropped silently.

Render reads `view_dir/_meta.json`; if `transcoder_version` differs from
the compiled constant OR `source_canonical_size_at_build` differs from
the current canonical's size, the cached view is treated as missing
and rebuilt.

## Render flow

### Render entry point (tier 3)

```rust
pub async fn render_into_working_v2(
    prior_segments: &[SegmentRow],
    active_segment: &SegmentRow,
    target_engine: Engine,
    target_family: Family,
    target_profile: &Profile,
    conversation_cwd: &str,
    central: &CentralPaths,
) -> Result<RenderOutcome, RenderError> {
    let Some(session_id) = &active_segment.engine_session_id else {
        return Ok(RenderOutcome::SkippedFirstTurn);
    };

    let working_main = working_main_path(target_engine, target_profile, conversation_cwd, session_id);
    let working_sidecar = working_sidecar_root(target_engine, target_profile, conversation_cwd, session_id);
    fs::create_dir_all(working_main.parent().unwrap())?;

    let tmp_main = working_main.with_extension("tmp");

    {
        let mut out = BufWriter::new(File::create(&tmp_main)?);

        if target_engine == Engine::Codex {
            write_codex_preamble(&mut out, session_id, conversation_cwd, target_profile)?;
        }

        for seg in prior_segments {
            let src_engine = Engine::from_str(&seg.backend);
            let src_family = Family::parse(&seg.source_family).unwrap();

            // Pick policy_input: canonical OR transcoded view.
            let policy_input: PathBuf = if src_engine == target_engine {
                central.segment_canonical_path(&seg.id)
            } else {
                let view_dir = central.segment_view_dir(&seg.id, target_engine);
                if !view_is_current(&view_dir, &seg.id, central)? {
                    transcode_to(target_engine, TranscodeInput {
                        source_engine: src_engine,
                        source_events_jsonl: &central.segment_canonical_path(&seg.id),
                        source_sidecar_dir: &central.segment_sidecar_dir(&seg.id),
                        source_engine_session_id: seg.engine_session_id.as_deref().unwrap_or(""),
                        conversation_cwd,
                    }, &view_dir)?;
                }
                view_dir.join(target_engine.view_main_filename())
            };

            // Family policy on top.
            let policy = min_policy_for(src_family, target_family);
            apply_policy_to_target(policy_input.as_path(), policy, target_engine, &mut out)?;
        }

        out.flush()?;
    }

    // Sidecar (per-engine layout).
    sync_sidecar_for_target(prior_segments, target_engine, &working_sidecar, central)?;

    // Safety net.
    enforce_no_empty_overwrite(&tmp_main, &working_main)?;
    fs::rename(&tmp_main, &working_main)?;

    // Codex requires session registration. spike result decides if/how.
    if target_engine == Engine::Codex {
        register_codex_session(&working_main, session_id, target_profile)?;
    }

    let bytes = fs::metadata(&working_main)?.len();
    Ok(RenderOutcome::Rendered { working_bytes: bytes })
}
```

`apply_policy_to_target` takes a policy_input file (canonical or view)
and applies the family policy to it. For Verbatim it's a byte copy
(when target_engine matches the format the file is in — which is always
true because either the file is canonical-same-engine or it's a
transcoded view in target's shape). For StripReasoning it runs the
engine-specific strip routine (claude: existing `strip_reasoning`;
codex: a new `strip_codex_reasoning` that drops `response_item::Reasoning`).

### `sessionId` rewriting in tier 3 (claude)

Tier 1's "no sessionId rewriting" rule assumed every segment in a
conversation shared `conversation.session_uuid`. Tier 3 drops that
column; each segment now has its own `engine_session_id`. When
concatenating multiple claude segments into a single working file at
`<active_segment.engine_session_id>.jsonl`, every line's `sessionId`
should match `active_segment.engine_session_id`. Whether claude CLI
actually validates line-level sessionId at resume time is **empirically
unproven**; tier 1's spike proved only that `--resume <id>` works
against a file at `<id>.jsonl`. Conservative path: rewrite per-line.

Implementation:
- For every line being written into the claude working file:
  - Parse via `serde_json::Value` (NOT typed `ClaudeEvent`, to preserve
    unknown fields claude may add).
  - If the value is an object with a `"sessionId"` field, replace it
    with `active_segment.engine_session_id`.
  - Re-emit via `serde_json::to_string`. (Cost: ~50µs per line for
    typical-size events; acceptable given total transcripts are bounded
    by claude's context window.)
- Raw string substitution at the `"sessionId":"<old>"` literal level
  was considered and **rejected**: a malformed line containing the
  literal `"sessionId":"..."` in escaped JSON-in-a-string would be
  corrupted by literal substitution. Parse/re-emit is the safe choice.

This rewriting runs in:
- `apply_claude_segment` (the same-engine policy applier).
- The transcoder's codex→claude output sentinel pass (transcoder emits
  with sessionId = view_engine_session_id; render then rewrites).

**Empirical follow-up**: a small spike (one synthesized claude jsonl
with three lines that have different sessionId values) verifies whether
claude CLI rejects/warns or accepts and proceeds. Run before final
implementation; if claude tolerates mismatched sessionIds, the rewrite
becomes optional optimization rather than correctness requirement.

For codex, the equivalent issue doesn't arise: each rollout has
exactly one `session_meta`, our event-type-based preamble filter
removes per-segment session_metas, and the working file has exactly
one session_meta (the one written by `write_codex_preamble`) with the
right id.

### `min_policy_for` extension

Cross-engine via transcoder always drops reasoning (it has to). After
transcode, the resulting view file is in target engine's shape with no
reasoning blocks. The post-transcode family policy is therefore always
`Verbatim` for the cross-engine case (there's no reasoning left to
strip). For same-engine, behavior is unchanged.

Effective table:

| src.engine | src.family | dst.engine | dst.family | Effective policy |
|---|---|---|---|---|
| same | same | (n/a) | (n/a) | Verbatim |
| same | lax | same | strict | StripReasoning (existing) |
| same | strict | same | lax | Verbatim (existing) |
| different | (any) | different | (any) | transcode + Verbatim (transcoder already dropped reasoning) |

### Codex preamble

```rust
fn write_codex_preamble(
    out: &mut impl Write,
    session_id: &str,
    cwd: &str,
    profile: &Profile,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let session_meta = serde_json::json!({
        "type": "session_meta",
        "timestamp": now,
        "payload": {
            "id": session_id,
            "cwd": cwd,
            "originator": "anatta",
            "cli_version": "anatta-render",
            "timestamp": now,
            "model_provider": &profile.provider, // e.g. "openai" or a codex-compat name
        }
    });
    let turn_context = serde_json::json!({
        "type": "turn_context",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "cwd": cwd,
            "model": profile.model_override.as_deref().unwrap_or(""),
            "approval_policy": "never",
            "sandbox_policy": {"type": "danger_full_access"}
        }
    });
    writeln!(out, "{}", session_meta)?;
    writeln!(out, "{}", turn_context)?;
    Ok(())
}
```

This preamble is **only** for the working file. The central canonical
of a codex segment starts with codex's own `session_meta` (written by
codex itself), not this synthesized one. Render's `apply_policy_to_target`
for same-engine codex segments must filter **by event type**, not by
line index — real codex rollouts often interleave `event_msg`s
(`thread_name_updated`, `task_started`) and bootstrap `response_item`s
(developer / user messages) BEFORE the first `turn_context`. Specifically:

```rust
fn apply_codex_segment(src: &Path, out: &mut impl Write) -> Result<()> {
    let mut first_session_meta_seen = false;
    let mut first_turn_context_seen = false;
    for line in BufReader::new(File::open(src)?).lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        // Parse to identify event type. Use a cheap top-level lookup; do not
        // round-trip to typed CodexEventKind (which is strict and would fail
        // on unknown fields claude doesn't care about).
        let val: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| RenderError::ParseSource {
                line: line.to_owned(),
                source: e,
            })?;
        let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "session_meta" if !first_session_meta_seen => {
                first_session_meta_seen = true;
                continue;  // drop the canonical's bootstrap session_meta
            }
            "turn_context" if !first_turn_context_seen => {
                first_turn_context_seen = true;
                continue;  // drop the canonical's bootstrap turn_context
            }
            _ => {
                writeln!(out, "{}", line)?;
            }
        }
    }
    Ok(())
}
```

`RenderError::ParseSource` is a new variant tier 3 adds to the
existing `RenderError` enum (which today has `PolicyNotImplemented`,
`WouldEmptyOverwrite`, `Io`, `Sanitize`):

```rust
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    // ... existing variants ...
    #[error("malformed source line during render: {line}")]
    ParseSource {
        line: String,
        #[source]
        source: serde_json::Error,
    },
}
```

The "first occurrence of (session_meta, turn_context) is bootstrap;
subsequent occurrences are mid-session and preserved" rule was
empirically derived from 434 real codex rollouts (none of which had a
second session_meta; most had additional turn_contexts at every turn
boundary).

Bootstrap `response_item` rows that precede the first `turn_context`
(e.g., codex's own pre-built developer instructions; user system
messages emitted by codex internally before the first user turn) are
**preserved** by the rule above — they sit between session_meta and
the first turn_context but are kept because their type is
`response_item`, not session_meta/turn_context. Downstream codex
replay treats them as part of the conversation history; this is the
right semantics.

Cross-engine (transcoded view) codex segments **also** have a preamble
in their view file: the transcoder writes a self-consistent codex
rollout starting with its own `session_meta` + `turn_context` (so the
view is a valid standalone codex rollout file usable in isolation). The
same event-type filter therefore applies uniformly — whether the source
is codex canonical or a codex view, the first session_meta + first
turn_context are dropped on concatenation into the working file. The
transcoder's preamble emits session_meta.id = view_engine_session_id,
which is meaningful inside the view but discarded on concatenation.

### Codex working area: rollout injection vs prompt-injection fallback

**Pre-implementation spike required.** §10 below.

If spike confirms `thread/resume <id>` works on an externally-written
rollout file (with possible session_index registration):

- Render writes the main rollout + sub-agent rollouts under
  `<codex_profile.path>/sessions/<YYYY>/<MM>/<DD>/`.
- `register_codex_session` updates `session_index.jsonl` (and possibly
  `state_5.sqlite`) as needed.
- `BackendLaunch::Codex` carries `resume: Some(session_id)`; codex's
  `thread/resume` finds the rollout and replays.

If spike shows resume only works on rollouts codex itself wrote:

- Render writes a **transcript bundle** (one giant text block) instead
  of a real rollout.
- `BackendLaunch::Codex` uses `thread/start` (fresh) for any segment
  whose prior history contains foreign-engine segments.
- `turn/start.input[0]` is the **bundled transcript preamble**
  ("This conversation began on [claude]; the prior transcript follows:
  ..."), and `turn/start.input[1]` is the user's actual prompt. Order
  is consistent with the `prefix_input` shape detailed below.
- Sub-agents are not invokable in the new codex thread; their transcripts
  are referenced as inline quoted text in the preamble.

Both paths are described in code; the spike result picks one. The spec
ships with both stubs so the implementation can compile under either
choice.

## Lifecycle integration

### `/profile` swap (in-chat, cross-engine)

```
1. User invokes /profile in chat.
2. Picker lists ALL profiles regardless of backend.
   - Same-backend, same-family: no warning.
   - Same-backend, different family: ⓘ "reasoning will be stripped going forward".
   - Different backend: ⚠ "switching engine; reasoning blocks dropped from prior
     foreign-engine segments; tool calls and text preserved; sub-agents transcoded".
   - Picker confirmation prompt: "Switch to <X>? [y/N]".
3. On confirmation:
   a. Final absorb of active_seg under old profile.
   b. Codex-specific: also absorb sub-agent rollouts (§absorb-sub-agent).
   c. Delete old profile's working file + sidecar.
   d. UPDATE active_seg SET ended_at = now.
   e. INSERT new_seg
        ordinal = active_seg.ordinal + 1,
        profile_id = P_new.id,
        backend = P_new.backend,
        source_family = family_of(P_new),
        engine_session_id = NULL,            -- will be set on first turn
        transition_policy = '{"kind":"verbatim"}'   -- post-transcode
        last_absorbed_bytes = 0,
        render_initial_bytes = 0
   f. Render under P_new (renders ALL prior segments, applying
      transcoder-if-needed + family-policy):
        - For each prior seg:
            if src_engine == P_new.backend:
                use canonical; apply min_policy_for(seg.source_family, family_of(P_new))
            else:
                ensure view cache exists in central (transcode if missing/stale);
                apply Verbatim on the view (transcoder already stripped reasoning)
        - Concatenate into new_seg's working file (in P_new.backend's shape).
   g. Set new_seg.{last_absorbed_bytes, render_initial_bytes} = render_size.
   h. Update Session to drive new_seg under P_new.
   i. Continue chat loop.
```

Rollback semantics: same as tier 1 (wrap DB writes in a transaction;
commit only after render returns Ok; file-system deletes of old working
file happen post-commit).

### First-turn flow (cross-engine considerations)

When a user creates a fresh conversation, segment 0's backend is set
from the chosen profile. Segment 0 has no prior segments, so render is
a no-op until the first turn establishes `engine_session_id`. No
transcoder ever runs at this stage.

### `anatta send --resume <id>` (cross-engine)

`send --resume` looks up a conversation by `backend_session_id` (legacy
column, kept for compat) or `engine_session_id` of any segment.

- If looked up by legacy `backend_session_id`: matches the segment whose
  `engine_session_id == that_id` (or the conversation row's deprecated
  field if migration hasn't touched it).
- Active segment determines the backend to spawn under.
- Render runs same as chat — produces a working file in the active
  segment's backend's shape, transcoding foreign-engine prior segments.
- Single-turn spawn → absorb → release lock.

There is no profile-swap in `send`. To swap, use chat.

### Crash recovery

Identical to tier 1, except absorb's sub-agent extension (for codex
segments only) runs as part of recovery too.

## Provider env layer

### Current state (`crates/anatta-runtime/src/profile/providers.rs`)

`PROVIDERS` registry has `backend: "claude"|"codex"` per row but env
vars are all Anthropic-namespaced. Codex profiles use `OPENAI_API_KEY`
via a separate code path in `spawn::codex.rs`.

### Tier 3 changes

`ProviderEnv::build` becomes backend-aware:

```rust
impl ProviderEnv {
    pub fn build(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
        match spec.backend {
            "claude" => Self::build_claude(spec, over, auth_token),
            "codex"  => Self::build_codex(spec, over, auth_token),
            other    => unreachable!("provider registry guarantees backend ∈ {{claude, codex}}; got {other}"),
        }
    }
}
```

`build_claude` is the existing logic (ANTHROPIC_BASE_URL,
ANTHROPIC_MODEL, etc.).

`build_codex` is new:

```rust
fn build_codex(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
    let mut vars = Vec::new();
    if let Some(v) = pick(&over.base_url, spec.base_url) {
        vars.push(("OPENAI_BASE_URL".into(), v));
    }
    vars.push(("OPENAI_API_KEY".into(), auth_token));
    if let Some(v) = pick(&over.model, spec.model) {
        vars.push(("CODEX_MODEL".into(), v));      // codex's preferred override
    }
    // codex doesn't have opus/sonnet/haiku tier overrides; ignore those.
    for (k, v) in spec.extra_env { vars.push((k.to_string(), v.to_string())); }
    Self { vars }
}
```

The `Overrides` struct stays unchanged; codex-side just ignores fields
that don't apply (opus/sonnet/haiku tier overrides).

`PROVIDERS` registry gains codex entries beyond `openai` only as users
request them. Out of scope: building out a full codex-compat provider
list (analogous to the claude-compat tier 2/3 providers).

## CLI surface

### `/profile` slash picker

Replace `apps/anatta-cli/src/chat/slash.rs::handle_profile`:

- List all profiles (no backend filter).
- Decorate each row with:
  - `★` = currently active.
  - `(same)` = same backend + family as current.
  - `(family)` = same backend, different family.
  - `(engine)` = different backend.
- On cross-engine pick, require an explicit confirmation:

```
Switching to a different engine (claude → codex).
Effect on prior history:
  - Text and tool calls/results are preserved.
  - Reasoning blocks (thinking/reasoning items) are dropped from foreign-engine segments.
  - Sub-agent transcripts are transcoded.
Confirm? [y/N]:
```

- Remove the rejecting branch (`if new_profile.backend != current.backend → return Continue`).

### `anatta chat new --profile <p>`

No change. Segment 0's backend is taken from the chosen profile.

### `anatta send --resume <id>`

No change at the CLI surface. The lookup logic in
`apps/anatta-cli/src/send.rs` is updated to find segments by
`engine_session_id` regardless of conversation backend.

### `anatta conversation list`

Display columns gain a "current backend" computed from the active
segment, not from the dropped `conversations.backend` column. Mixed-
engine conversations show a `Σ` marker.

## Migration

### Migration 0007 (additive + Rust backfill + Rust-guarded DROP)

**The migration sequence cannot be expressed as plain
`sqlx::migrate!` because (a) `sqlx::migrate!` runs all SQL migrations
in one pass before any application code runs, leaving no place to
slip the Rust backfill in; (b) SQLite's `RAISE()` is valid only
inside triggers, so a "validate-then-DROP" guard cannot be expressed
in a plain SELECT.** Tier 3 therefore uses a dedicated `MigrationDriver`
that orchestrates three steps:

```rust
// crates/anatta-store/src/migrate.rs
pub async fn run_tier3_migration(pool: &SqlitePool) -> Result<()> {
    // Step 1: SQL additive (0007a).
    sqlx::migrate!("./migrations").run(pool).await?;
    //         (0007a runs as part of the normal migrate set above)

    // Step 2: Rust backfill (idempotent).
    if !backfill_already_complete(pool).await? {
        precheck_no_orphan_profile_ids(pool).await?;
        backfill_cross_engine(pool).await?;
        mark_backfill_complete(pool).await?;
    }

    // Step 3: Rust-guarded destructive DROP (effectively 0007b).
    if !drop_already_complete(pool).await? {
        ensure_backfill_complete(pool).await?;          // refuses if step 2 didn't run
        ensure_no_null_backend_rows(pool).await?;       // sanity check
        execute_destructive_drop(pool).await?;          // ALTER TABLE ... DROP COLUMN
        mark_drop_complete(pool).await?;
    }
    Ok(())
}
```

`anatta_migration_state` is a small table created by 0007a to hold
the completion markers:

```sql
CREATE TABLE IF NOT EXISTS anatta_migration_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

**Locking**: `run_tier3_migration` acquires the global anatta migration
lock (a file-level flock under `<anatta_home>/runtime-locks/migration.lock`)
for the duration. Two concurrent anatta startups serialize on it.

**FK pragma**: As of sqlx 0.8 (anatta's current version), `PRAGMA
foreign_keys = ON` **is the default** for `SqliteConnectOptions`. Round-2
review correction: we don't need to add a pragma call; the `ON DELETE
RESTRICT` on `conversation_segments.profile_id` is enforced from the
moment `Store::open` returns. The orphan-precheck below is kept anyway
as a belt-and-suspenders safety: if a legacy database somehow has FK
disabled (e.g., manual sqlite3 edits), the precheck catches it.

Orphan precheck:

```sql
SELECT cs.id, cs.profile_id
FROM conversation_segments cs
LEFT JOIN profile p ON p.id = cs.profile_id
WHERE p.id IS NULL;
```

If any rows return, the backfill aborts loudly with a list of orphan
segment ids; the user must resolve manually (delete orphan segments or
restore the missing profile) before retrying. This is non-destructive
and reversible.

```sql
-- migrations/0007a_cross_engine_additive.sql (runs as part of sqlx::migrate!)
BEGIN;
ALTER TABLE conversation_segments ADD COLUMN backend TEXT NOT NULL DEFAULT 'claude';
ALTER TABLE conversation_segments ADD COLUMN engine_session_id TEXT;
CREATE TABLE IF NOT EXISTS anatta_migration_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
COMMIT;
```

Rust backfill, run on startup after sqlx migrations complete, **before
any other code path opens conversation rows**. Idempotent:

```rust
async fn backfill_cross_engine(store: &Store) -> Result<()> {
    let mut tx = store.begin().await?;

    // 1. For each segment, copy its profile's backend into segment.backend.
    sqlx::query!(
        "UPDATE conversation_segments
             SET backend = (SELECT backend FROM profile WHERE profile.id = conversation_segments.profile_id)
             WHERE backend = 'claude' OR backend IS NULL"
        // NOTE: this overwrites the default 'claude' even where it's
        // actually claude — idempotent.
    ).execute(&mut *tx).await?;

    // 2. For each segment, copy conversations.session_uuid into segment.engine_session_id
    //    for the earliest segment of each conversation (which is the one that "owns" the legacy id).
    sqlx::query!(
        "UPDATE conversation_segments AS cs
             SET engine_session_id = (
                 SELECT session_uuid FROM conversations
                  WHERE conversations.id = cs.conversation_id
             )
             WHERE cs.ordinal = 0
               AND cs.engine_session_id IS NULL"
    ).execute(&mut *tx).await?;

    // 3. Mark backfill complete.
    sqlx::query("INSERT OR REPLACE INTO anatta_migration_state (key, value) VALUES ('0007_backfill', 'done')")
        .execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(())
}
```

Step 3 (Rust-guarded destructive DROP) executes after the backfill
marker is set. The driver re-validates state, then runs the DROPs in a
single transaction:

```rust
async fn execute_destructive_drop(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("ALTER TABLE conversations DROP COLUMN backend")
        .execute(&mut *tx).await?;
    sqlx::query("ALTER TABLE conversations DROP COLUMN session_uuid")
        .execute(&mut *tx).await?;
    sqlx::query("INSERT OR REPLACE INTO anatta_migration_state (key, value) VALUES ('0007_drop', 'done')")
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn ensure_backfill_complete(pool: &SqlitePool) -> Result<()> {
    let done: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM anatta_migration_state WHERE key = '0007_backfill'"
    ).fetch_optional(pool).await?;
    match done.as_deref() {
        Some((v,)) if v == "done" => Ok(()),
        _ => Err(anatta_store::Error::MigrationBlocked(
            "0007 backfill incomplete; refusing to drop conversations columns. \
             Re-run anatta normally to retry the backfill."
        )),
    }
}

async fn ensure_no_null_backend_rows(pool: &SqlitePool) -> Result<()> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_segments WHERE backend IS NULL"
    ).fetch_one(pool).await?;
    if n > 0 {
        return Err(anatta_store::Error::MigrationBlocked(
            "conversation_segments rows with NULL backend exist; backfill incomplete"
        ));
    }
    Ok(())
}
```

**Interrupt safety**: at any point, the migration sequence is
re-runnable. If anatta crashes:
- Between 0007a and backfill: backfill runs on next startup. SQL is
  re-applied trivially (migrations table records 0007a as done).
- Mid-backfill: idempotent. SET WHERE backend IS NULL is a no-op for
  already-backfilled rows; UPDATE engine_session_id WHERE NULL is the same.
- Between backfill and 0007b: 0007b runs on next startup, sees the
  done marker, proceeds.
- During 0007b: SQLite transaction rolls back; user retries.

**FK enable in steady state**: independent of migration, `Store::open`
gains `PRAGMA foreign_keys = ON` via `SqliteConnectOptions::pragma(...)`.
This is a small standalone change (not gated on tier 3) and should
ship before 0007.

### Central store layout migration

For each existing segment dir:

```
1. Read segment.backend (after backfill).
2. If meta.json absent, write it with {backend, source_family, ...}.
3. Existing events.jsonl is already in the right shape (segment was
   single-backend in tier 1/2).
4. views/ directory is not created — lazy on first cross-engine render.
```

No file rewrites are needed; tier 1/2 segments are naturally compatible
with tier 3's canonical-per-segment-engine invariant.

## Codex resume spike (RESOLVED 2026-05-13)

### Result

**RolloutInjection works.** Codex honors `thread/resume <thread_id>`
against a rollout file anatta wrote into `<CODEX_HOME>/sessions/.../`.
No prior state_5.sqlite registration is required: codex auto-inserts a
`threads` row from the rollout's `session_meta` on first resume.

Empirical evidence from spike (codex-cli 0.125.0):
- Hand-crafted 4-line rollout: `session_meta` + `turn_context` + user
  `response_item` + assistant `response_item`.
- `thread/resume {threadId: "019c1234-0000-7000-8000-000000000001"}`
  returned `result.thread.id = our id`, `result.thread.path = our file`,
  `result.thread.preview = our user text`.
- A follow-up `turn/start` with the prompt "what is my favorite
  color?" produced the assistant response **literally containing
  `anatta-spike-purple-7`** — the unique sentinel string from our
  rollout. inputTokens = 27913 confirms full replay (the rollout +
  codex's own injected permissions/skills/apps preamble).

Known harmless: codex logs one
`ERROR codex_core::session: failed to record rollout items: thread X not found`
to stderr per resume (a pre-registration ordering quirk inside codex
session bootstrap). Does not affect resume correctness; ignore in
production by filtering codex stderr for this exact message.

### What this resolves

- §4 working area: rollout-injection path is the actual implementation;
  PromptInjection fallback section is informational only and may be
  removed in a final spec revision.
- §3.8 codex sub-agent absorb: still required (we copy sub rollouts
  into central sidecar from `state_5.sqlite` lookup) because the same
  auto-registration applies to sub-agent threads — codex will register
  them when resumed, but our prior absorb still needs them locally.
- §10 (this section): spike is no longer pre-implementation gate.

### Goal (historical, for reference)

Determine the minimum set of artifacts codex needs to honor
`thread/resume <thread_id>` against a rollout file anatta wrote.

### Setup

```
export CODEX_HOME=/tmp/anatta-codex-spike
mkdir -p $CODEX_HOME/sessions/2026/05/13
# Copy a real recent rollout from ~/.codex/sessions/ for reference shape.
cp ~/.codex/sessions/2026/<latest dir>/rollout-*.jsonl /tmp/codex-real-rollout.jsonl
# Also copy state_5.sqlite to inspect schema:
cp ~/.codex/state_5.sqlite /tmp/codex-state-schema.sqlite
sqlite3 /tmp/codex-state-schema.sqlite ".schema threads"
sqlite3 /tmp/codex-state-schema.sqlite ".schema thread_spawn_edges"
```

Hand-craft a minimal rollout:

```
$CODEX_HOME/sessions/2026/05/13/rollout-2026-05-13T12-00-00-019c1234-5678-9abc-def0-000000000001.jsonl:
  {"type":"session_meta","timestamp":"2026-05-13T12:00:00Z","payload":{"id":"019c1234-5678-9abc-def0-000000000001","cwd":"/tmp","originator":"anatta-spike","cli_version":"spike","timestamp":"2026-05-13T12:00:00Z","model_provider":"openai"}}
  {"type":"turn_context","timestamp":"2026-05-13T12:00:00Z","payload":{"cwd":"/tmp","model":"gpt-5","approval_policy":"never","sandbox_policy":{"type":"danger_full_access"}}}
  {"type":"response_item","timestamp":"2026-05-13T12:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
  {"type":"response_item","timestamp":"2026-05-13T12:00:02Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}
```

### Procedure

```
1. Spawn `codex app-server` against this CODEX_HOME.
2. Send: initialize → initialized → thread/resume {threadId: "019c1234-..."}.
3. Observe the response. The current spawn driver does not validate the
   returned thread.id against the requested one (spawn/codex.rs:545) —
   the spike must do that check explicitly to detect "codex silently
   started a fresh thread instead of resuming".
4. Decision tree:
   - If `result.thread.id == requested id` AND a subsequent turn/start
     produces an assistant response that references the prior fake "hello":
     → rollout-injection works with sessions/ alone. Done.
   - If `result.thread.id != requested id` OR no reference to prior turn:
     → codex didn't actually resume our content. Try:
       a. Add a `threads` row in state_5.sqlite with (thread_id, rollout_path).
          Schema discovered from inspecting `~/.codex/state_5.sqlite`.
       b. Retry. If now resumes correctly → rollout + state_5 row is the requirement.
       c. If still no: add thread_spawn_edges row (root: no parent_thread_id).
          Retry.
       d. If still no: declare rollout-injection unviable → fall back to
          PromptInjection.

5. Also exercise sub-agent path: spawn a real codex sub-agent through the
   real codex CLI (e.g., via an agent-spawning prompt), observe:
   - The on-wire shape of the spawn event (response_item vs event_msg
     vs collab stream item).
   - The state_5.sqlite rows written (threads / thread_spawn_edges).
   - The location of the sub-agent's rollout file.
   This resolves the open question in §3.4 about Task → codex mapping.

6. Record findings under §"Spike result" appended to this design doc.
   Document:
   - Exact state_5.sqlite schema for `threads`, `thread_spawn_edges`,
     and any other table involved in resume.
   - Whether session_index.jsonl is necessary.
   - The empirical on-wire shape of sub-agent spawning.
```

### Fallback if rollout-injection unviable

The codex working-area writes never happen; instead the render output
is a transcript bundle ingested as the first user-message in
`turn/start`. Implementation:

```rust
match codex_resume_mode {
    CodexResumeMode::RolloutInjection => {
        // ... existing render path ...
        codex_launch.resume = Some(session_id);
    }
    CodexResumeMode::PromptInjection => {
        // No file write to sessions/. Instead build the prompt bundle.
        codex_launch.resume = None;
        codex_launch.prefix_input = Some(build_prompt_bundle(prior_segments, conversation_cwd)?);
    }
}
```

`prefix_input` is a new field on `CodexLaunch` (today `prompt` is a
single string; we add a separate `Option<String>` field whose contents
become the `turn/start.input[0]` if non-None, with the user's actual
prompt as `input[1]`). Both `CodexLaunch::launch` (one-shot send) and
`PersistentCodexSession::send_turn` (persistent chat) must consume the
new field; in `send_turn`, prefix_input is consumed on the first turn
only — subsequent turns send only the user's prompt.

**Risk**: codex may truncate `turn/start.input` if the total token
count exceeds the model context window. Document this as a degradation
mode; warn the user in the cross-engine `/profile` confirmation that
"if prior history exceeds model context, the new engine will see a
truncated view".

## Edge cases

### 1. First turn of segment 0 on cross-engine conversation

There is no cross-engine state to render — segment 0 is fresh. Path is
identical to tier 1.

### 2. Cross-engine swap on a conversation that has never advanced past first turn

`active_seg.engine_session_id` is NULL. Render is skipped. New segment
is inserted; first turn under the new engine creates everything fresh.
The "prior segments" loop sees segment 0 with empty canonical → skipped.

### 3. Swap, then swap back, then swap again

Three (or more) segment boundaries; transcoder caches accumulate for
ended segments. Active segment is always in the current engine. Render
walks all priors and uses cache hits where possible.

### 4. User edits transcode-cache files manually

Out of scope. The `_meta.json.source_canonical_size_at_build` check
detects only canonical-side changes. Hand-editing a view is on the
user's head; render does not verify view content integrity.

### 5. Sub-agent transcript that itself contains a sub-agent (depth > 1)

Recursion handles it: each sub-agent's view is transcoded via the same
`transcode_to` call recursing on the sub's transcript file. Sub of sub
gets `view_sub_thread_id(parent_sub_view_id, ...)`. Deterministic
through any depth.

### 6. Sub-agent uses a tool with the same call_id as the parent

Tool call ids are namespaced via the prefix in `map_tool_call_id`
(`anatta-cc-` / `anatta-cx-`) but the *source* ids are claude's
`toolu_*` (random) and codex's `call_*` (random), so collisions
between parent and child are not realistic. If they ever happen, the
mapping is still 1:1 within a single transcript file because the source
files were independent.

### 7a. /profile-swap requested mid-turn

User invokes `/profile` while a streaming response is still arriving.
**Tier 3 rejects this**: chat REPL only allows `/profile` (or any slash
command) when no turn is active. Same as tier 1's existing precondition
in `chat/runner.rs`; tier 3 adds no new mid-turn path.

**Where the guard lives**: the chat REPL's existing precondition
(slash commands only fire when no turn is active) is the primary
guard. Tier 3 adds a defensive check at the runtime layer too, via a
new `Session::is_idle()` accessor:

```rust
impl Session {
    pub async fn is_idle(&self) -> bool {
        match self {
            Session::Claude(_) => true, // claude has no inter-turn live state
            Session::Codex(c)  => c.is_idle().await,
        }
    }
}

impl PersistentCodexSession {
    pub async fn is_idle(&self) -> bool {
        self.active_turn.lock().await.is_none()
    }
}

impl Session {
    pub async fn swap(&mut self, new_launch: BackendLaunch) -> Result<(), SwapError> {
        if !self.is_idle().await {
            return Err(SwapError::TurnActive);
        }
        // ... existing same-backend/cross-backend branches ...
    }
}
```

`PersistentCodexSession` already maintains `active_turn:
Arc<Mutex<Option<ActiveTurn>>>` (see spawn/codex.rs:194-195), so
`is_idle` is a one-liner. `Session::Claude` is per-turn-spawn — there
is no session-level "active turn" state at all, so `is_idle = true`.

### 7. Codex segment exists but `register_codex_session` fails

Render returns the error; chat surfaces it; segment is rolled back per
tier 1 transaction semantics. The codex working file written but not
registered is harmless garbage; a janitor on next startup could clean
stray sessions/ files older than 24h with no DB reference.

### 8. Transcoder version mismatch on a frequently-accessed segment

First render after upgrade rebuilds the view. Subsequent renders see
the matching version. Worst case: one slow render per segment per
transcoder upgrade.

### 9. Profile that's been deleted between segments

`ON DELETE RESTRICT` on `conversation_segments.profile_id` already
prevents this in tier 1. Unchanged.

### 10a. Concurrent render of the same conversation by two anatta processes

`SessionLock` (tier 1) is a non-blocking `try_acquire` (see
crates/anatta-runtime/src/session_lock.rs); a second anatta opening the
same conversation **fails fast** with `LockError::AlreadyHeld` rather
than waiting. Tier 3 takes the lock at the same point as tier 1
(before render, before any view-cache write). Two anatta processes
opening the same conversation see one succeed and the other refused.
Transcode `.tmp` paths are keyed by the locked conversation's segment
id; even if the lock were ever made blocking in the future, atomic
rename ensures readers see only the completed view.

If a third process tries to view a conversation read-only (out of scope
for tier 3 but anticipated): cache reads are safe lock-free because
view-cache directories are written via atomic rename. A reader sees
either the old view or the new view, never partial bytes.

### 10b. Corrupt canonical events.jsonl

Tier 1's `Verbatim` policy is a byte copy and never inspects content.
Tier 3 adds a transcoder that parses the source. If parse fails:
- Cross-engine render: transcoder returns `TranscodeError::Parse{line, source}`,
  render propagates. Chat surfaces the error; user can decide to delete
  the corrupt segment manually.
- Same-engine render via `Verbatim`: bytes pass through unchanged
  (status quo). The downstream engine's parser will see the corruption.
- Same-engine render via `StripReasoning` or `apply_codex_segment`:
  parse failure is now surfaced (today's `strip_reasoning` already
  errors loudly; new `apply_codex_segment` does too).

A "corrupt segment" CLI repair tool is out of scope.

### 10. Mixed-engine `anatta send --resume`

User invokes `anatta send --resume <legacy backend_session_id>`. We
look up the conversation; active segment may be in a different engine
than the user expected. Behavior: spawn under active segment's engine;
print a notice if `--resume` is ambiguous (e.g., reverse-lookup matched
more than one segment).

## Testing strategy

### Unit (fixture-based, no DB, no LLM)

- `transcode/claude_to_codex` fixtures (one per event-category mapping
  row from §3.4). Input: minimal claude jsonl line; expected output:
  exact codex line bytes.
- `transcode/codex_to_claude` fixtures (one per row from §3.5).
- `id_mint` property tests: determinism (same input → same output),
  namespace isolation (anatta-minted ids never collide with realistic
  engine-native ids).
- Sub-agent recursion fixtures (one depth-1, one depth-2).
- `render::view_is_current` cache-key tests.
- `apply_policy_to_target` boundary cases (empty file, codex-canonical-with-preamble-skip, claude StripReasoning).

### Integration (DB + filesystem, no LLM)

- Tier 1/2 swap (claude → claude same family) → unchanged behavior.
- Tier 1/2 swap (claude lax → claude strict) → StripReasoning, unchanged.
- New: claude → codex swap → transcoder runs, view appears in central,
  codex working file is correctly shaped (assert via parser
  round-trip).
- New: codex → claude swap → reverse direction.
- New: claude → codex → claude. After the second swap, segment 0
  (claude) is rendered from its canonical directly; segment 1 (codex)
  is rendered via views/claude/.
- New: cache hit second time around (no rebuild on second render of
  same view).
- New: bump TRANSCODER_VERSION → cache invalidates and rebuilds.

### End-to-end (real backends, single profile pair)

Requires user supervision (token cost, real auth).

- Live `claude` chat, send one user prompt, get one assistant turn,
  `/profile` to a codex profile, confirm cross-engine swap dialog,
  send another user prompt, assert the codex response references the
  prior claude turn correctly.
- Same in reverse direction (codex first, swap to claude).
- Three-way: claude → codex → claude with two turns each.

### Codex resume spike

§10 above. Runs first; gates whether RolloutInjection or
PromptInjection is the codex working-area strategy.

## Migration plan summary

| Step | Action | Reversible? |
|---|---|---|
| 1 | Apply migration 0007 step 1 (ADD COLUMN backend, engine_session_id) | Yes (drop columns) |
| 2 | Run Rust backfill | Yes (clear new columns) |
| 3 | Apply migration 0007 step 2 (DROP COLUMN conversations.backend, session_uuid) | **No** (data loss; before this step, take a DB backup) |
| 4 | Deploy transcoder module + render v2 + new Session::swap allowing cross-backend | Yes (code revert) |
| 5 | Deploy /profile UI changes | Yes |
| 6 | First real cross-engine swap | n/a |

Step 3 is the only point of no return. Before applying, anatta refuses
to start if a backup file ≥ 24h old is the only one present (so the
operator must have a recent one).

## What's deferred to a future tier

- Live AgentEvent-stream cross-engine handoff (intra-segment).
- Cross-engine sub-agent thread continuation (re-addressing the same
  sub-agent across engines).
- Codex provider compat registry (codex-side analogue of tier 2/3
  claude proxies).
- Server-side sync, multi-host coordination.
- Cross-engine compact bridging (LLM-summarize the foreign-engine
  segment instead of transcoding it).

## Open questions resolved during brainstorm

| Question | Decision | Rationale |
|---|---|---|
| Same conversation or fork? | Same conversation, Invariant 1 dropped | User wants the in-chat experience to feel like one thread |
| Canonical format per segment? | Source engine native | Preserves max info; transcoded views are derived |
| Cross-engine reasoning? | Drop unconditionally | Signatures / encrypted_content can't be made valid for the other engine |
| Tool calls across engines? | 1:1 structural map, id prefix-namespaced | Preserves semantic continuity; new engine sees a complete tool call/result pair |
| Sub-agents? | Recursive transcode (claude Task ↔ codex CollabAgent*) | Both engines have isomorphic sub-agent concepts; flatten would lose info needlessly |
| Cache strategy? | Lazy per-target, invalidated on transcoder version bump | Same-engine users never pay cost; ended segments' caches are stable |
| Codex working area? | Spike-dependent; rollout-injection preferred, prompt-injection fallback | Codex's resume mechanism is opaque; design supports both |

## Pre-implementation checklist

- [ ] Codex resume spike (§10) completed; rollout vs prompt fallback chosen;
      state_5.sqlite schema documented.
- [ ] Codex sub-agent spawn empirical shape captured (response_item vs
      event_msg vs stream collab item).
- [ ] Claude sub-agent file naming and parent linkage convention
      verified against real sub-agent transcripts.
- [ ] Claude sessionId-line-mismatch tolerance test run (does claude
      CLI accept resume of a jsonl whose lines have mixed sessionIds?).
- [ ] First round of codex review folded in (this revision).
- [ ] Second round of codex review on this revised spec (to catch new
      issues introduced by the revisions).
- [ ] DB backup procedure documented for migration 0007b.
- [ ] Cross-engine swap confirmation dialog text finalized with user.
- [ ] Test fixture files prepared for both transcode directions, plus
      sub-agent cases.
- [ ] `PRAGMA foreign_keys = ON` patch landed (pre-tier-3 prerequisite).
- [ ] `Session::swap` mid-turn-rejection patch landed (pre-tier-3
      prerequisite).

After all items are checked, implementation can begin in the order:
transcoder module → render v2 → Session::swap loosening → migration
0007 → /profile UI → end-to-end test.
