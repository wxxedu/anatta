# anatta Roadmap

A Rust monorepo for orchestrating remote Claude Code sessions. Each
feature/intent runs in its own git worktree, supervised by a daemon that
pulls work from a server and surfaces progress as a card stream across
desktop and (eventually) mobile clients.

This document is the **single source of truth for what gets built when**.
Architectural decisions live in commit messages and code comments where they
matter; this file is about scope and sequencing.

---

## Architecture at a Glance

```
            ┌──────────────┐    ┌──────────────┐    ┌──────────────┐
            │ anatta-cli   │    │anatta-desktop│    │  mobile app  │
            │   (clap)     │    │   (iced)     │    │ (Flutter/SwiftUI, separate repo) │
            └──────┬───────┘    └──────┬───────┘    └──────┬───────┘
                   │   gRPC (tonic)    │                   │
                   └─────────┬─────────┴───────────────────┘
                             ▼
                     ┌──────────────────┐
                     │  anatta-server   │  ← message bus + state store
                     │   (axum + tonic, │     (SQLite in single-tenant mode)
                     │    sqlite)       │
                     └────────┬─────────┘
                              │ gRPC bidi stream
                              ▼
                     ┌──────────────────┐
                     │  anatta-daemon   │  ← orchestrator
                     │                  │
                     │  ┌────────────┐  │
                     │  │ worktree   │  │
                     │  │ runtime    │  │ ← agent CLI subprocess
                     │  │ guards     │  │ ← DAG executor
                     │  └────────────┘  │
                     └──────────────────┘
```

**Process model**: every node (cli, desktop, mobile, daemon) is a *client* of
the server. Daemon makes outbound bidi-stream connection — no inbound port
needed, NAT-friendly.

**Data ownership**:

| Lives on server | Lives on daemon |
|---|---|
| Intents (definition, DAG spec, deps) | Worktree on disk |
| Cards + history | Live agent process state |
| User feedback / replies | Per-intent FSM runtime state |
| Auth / users | Guard execution outcomes |

**Wire**: gRPC + protobuf (`.proto` schema in `proto/`, codegen via
`tonic-build` in `anatta-proto`). Auth via metadata interceptor (PAT in
single-tenant; JWT later).

---

## Phase 1 — Walking Skeleton

> **Goal**: One intent end-to-end, in-memory, no auth, no guards, no GUI.

### What works at end of Phase 1

```bash
$ anatta-server &
$ anatta-daemon --server localhost:50051 &
$ anatta-cli intent new "Add a hello function"
intent_42 created
$ anatta-cli intent watch intent_42
[setup]    creating worktree at /tmp/anatta/intent_42
[runtime]  spawning claude
[claude]   ...streaming output...
[done]     intent_42 → Done
```

### Crates opened in Phase 1 (9 total)

```
crates/
├── anatta-proto/         # gRPC codegen + minimal Intent/Event types
├── anatta-core/          # IntentState enum + transition fn (no guards yet)
├── anatta-worktree/      # create / cleanup
├── anatta-runtime/       # spawn claude, stream stdout/stderr
├── anatta-server-core/   # tonic service, in-memory store
└── anatta-daemon-core/   # one intent at a time, no scheduler

apps/
├── anatta-server/        # thin bin
├── anatta-daemon/        # thin bin
└── anatta-cli/           # `intent new`, `intent watch` (uses tonic directly,
                          #  no anatta-client crate yet)
```

### Out of scope for Phase 1 (intentionally)

- Guards (any of the 4 kinds) → Phase 2
- Inter-intent dependencies → Phase 2
- Persistence (SQLite) → Phase 2
- Auth → Phase 2
- Desktop GUI → Phase 2
- Card stream / user feedback → Phase 2 (Phase 1 events are append-only output)
- Merge workflow → Phase 2
- Multi-user → Phase 3

### Phase 1 exit criteria

- `cargo build --workspace` clean
- `cargo test --workspace` green
- The demo above runs on a Mac and Linux box
- One small repo used as a fixture; CI runs the demo end-to-end

---

## Phase 2 — Feature Complete

> **Goal**: A single user can run their actual development workflow on anatta.

### What works at end of Phase 2

- Define guard DAG per intent (Shell / ClaudeReview / HumanApproval / HTTPWebhook)
- Cards surface in desktop GUI when guards need user input; user replies
- Inter-intent hard dependencies enforced (A waits for B's merge to main)
- After guards pass, daemon invokes claude a second time to run the merge
  workflow (rebase, conflict resolution, open PR)
- SQLite persists everything; server restart preserves state
- PAT auth on all RPCs

### Crates opened in Phase 2 (4 new, 13 total)

```
crates/
├── anatta-guards/        ← NEW: 4 guard impls + DAG executor
└── anatta-client/        ← NEW: shared tonic client SDK
                          #  (extracted from Phase 1 cli, now also used by desktop)

apps/
└── anatta-desktop/       ← NEW: iced GUI
```

### Already-existing crates that grow in Phase 2

- `anatta-proto`: Card / Guard / Dependency messages added
- `anatta-core`: dependency-graph algorithms, Card FSM, Guard FSM
- `anatta-server-core`: sqlx + SQLite schema + migrations, auth interceptor,
  dependency scheduler
- `anatta-daemon-core`: merge workflow stage, guard trigger, scheduler-aware
  state machine

### Phase 2 exit criteria

- A real feature gets shipped through anatta end-to-end (intent → claude →
  guards → merge → PR merged)
- Two intents with a hard dependency are scheduled correctly
- Server restart mid-flight does not lose intent state
- Desktop client can replace cli for daily use

---

## Phase 3 — Ship-Ready

> **Goal**: Other people can self-host. Mobile teams can build clients.
> Author can begin building a hosted service.

### What works at end of Phase 3

- `docker-compose up` starts a self-hostable instance
- Multi-user data model in place (single-tenant default still works)
- `proto/anatta/v1/` published to a buf registry or mirrored repo so mobile
  teams can codegen
- Documentation: README, ARCHITECTURE.md, CONTRIBUTING.md, deployment guide

### No new crates in Phase 3

All work in Phase 3 is hardening, multi-tenancy, deployment, and docs.

### Phase 3 exit criteria

- Someone other than the author successfully self-hosts
- A Flutter or SwiftUI client (in a separate repo) successfully consumes
  the published schema and connects
- License, security disclosure, and contribution policies in place

---

## Discipline: Don't Open Crates Early

The reason for the phasing above is to **avoid stale package skeletons**. A
crate that gets `lib.rs` with a `todo!()` in week 1 and isn't touched again
until month 4 is worse than not existing — its presence implies a contract
that hasn't actually been designed.

**Rule**: a crate gets opened only in the phase that fills it with real code.
If you find yourself adding a placeholder dependency to an empty crate "for
later", stop and put the crate in a later phase.

---

## Out of Scope (across all phases)

- Plugin / WASM extensibility for guards → not until guards as enum proves
  insufficient
- Web client (browser) → not planned; gRPC + grpc-web bridge cost outweighs
  benefit while desktop + mobile cover use cases
- Real-time collaboration on a single intent → out of scope; intents are
  single-author
