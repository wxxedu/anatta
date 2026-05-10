# Provider Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a structured provider system so anatta can route Claude-compatible third-party backends (DeepSeek, Kimi, MiniMax, Z.AI) without changing parsers/spawn logic. Adding a new provider should be a single PR that appends one entry to a const table.

**Architecture:** Split the existing `BackendKind` axis (which CLI binary) from a new `Provider` axis (which API endpoint). `ProviderSpec` is a static const table in `anatta-runtime`. Each `ProviderSpec` declares Anthropic-canonical env-var defaults as named optional fields (`model`, `default_opus_model`, etc.) plus an `extra_env` slice for vendor-specific extras. Profiles hold per-row overrides for those same fields. At spawn time, `ProviderEnv::build` resolves `override > spec > unset` and the result is injected into `ClaudeLaunch`.

**Tech Stack:** Rust 2024, sqlx 0.8 (sqlite, offline-mode via committed `.sqlx/`), tokio, clap, dialoguer, keyring.

**Workflow conventions:**
- This codebase uses `SQLX_OFFLINE=true` (set in `.cargo/config.toml`). Any SQL change requires running `./scripts/sqlx-prepare.sh` to regenerate `crates/anatta-store/.sqlx/`. Commit that directory.
- Tests live alongside production code in `mod tests { ... }` blocks. Integration tests live in `crates/<crate>/tests/`.
- Commit at the end of each task. Build + test must be green before committing.

---

## File Structure

**New:**
- `crates/anatta-runtime/src/profile/providers.rs` — provider registry, `ProviderSpec`, `Overrides`, `ProviderEnv` builder
- `crates/anatta-store/migrations/0002_profile_provider.sql` — adds `provider`, 7 override columns

**Modified:**
- `crates/anatta-runtime/src/profile/mod.rs` — wire in `providers` submodule, re-export public types
- `crates/anatta-store/src/profile.rs` — extend `ProfileRecord` / `NewProfile`, update queries
- `crates/anatta-runtime/src/spawn/claude.rs` — add `provider: Option<ProviderEnv>` field, inject env vars
- `crates/anatta-runtime/tests/spawn_mock.rs` — new test for env-var injection
- `apps/anatta-cli/src/profile.rs` — interactive provider picker, new flags, login guard, updated `show`/`list`
- `apps/anatta-cli/src/auth.rs` — no signature change; CLI does the supported-auth check upstream

`apps/anatta-cli/src/auth.rs` only changes if needed (decision: do the login-guard check in `profile.rs` to keep `auth.rs` dumb).

---

## Task 1: Provider Registry + Resolved Env Builder

**Files:**
- Create: `crates/anatta-runtime/src/profile/providers.rs`
- Modify: `crates/anatta-runtime/src/profile/mod.rs`

This task adds the static provider data + the resolution function that produces an env list from `(spec, overrides, token)`. No store / spawn / CLI changes yet.

- [ ] **Step 1.1: Create `providers.rs` with the type skeleton (no data, no impl)**

Create `crates/anatta-runtime/src/profile/providers.rs`:

```rust
//! Provider registry: maps a provider id ("anthropic" / "deepseek" / "kimi" / ...)
//! to its Anthropic-namespace env-var defaults. Profiles store per-row
//! overrides; [`ProviderEnv::build`] resolves the layered values into a
//! flat env list that [`crate::spawn::ClaudeLaunch`] injects into the child
//! process.
//!
//! Adding a new provider = appending one entry to [`PROVIDERS`].

/// Display priority (T1 = always first in pickers). Functional only as
/// a sort key — no behavioral difference between tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    T1,
    T2,
    T3,
}

/// Static description of one provider. All env-var fields default to
/// `None`; `None` means "do not set this env var at spawn time" (i.e.
/// claude-cli's own default applies). Vendor-specific extras that don't
/// fit the Anthropic namespace go in `extra_env`.
#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id:           &'static str,                  // "anthropic", "deepseek", ...
    pub display_name: &'static str,                  // "Anthropic", "DeepSeek", ...
    pub backend:      &'static str,                  // "claude" / "codex"
    pub tier:         Tier,
    /// Auth methods supported by this provider. CLI rejects any choice
    /// not in this list. Currently "login" / "api_key".
    pub supported_auth: &'static [&'static str],

    // ── Anthropic-canonical env vars (None = don't set) ──────────────
    pub base_url:             Option<&'static str>,  // ANTHROPIC_BASE_URL
    pub model:                Option<&'static str>,  // ANTHROPIC_MODEL
    pub small_fast_model:     Option<&'static str>,  // ANTHROPIC_SMALL_FAST_MODEL
    pub default_opus_model:   Option<&'static str>,  // ANTHROPIC_DEFAULT_OPUS_MODEL
    pub default_sonnet_model: Option<&'static str>,  // ANTHROPIC_DEFAULT_SONNET_MODEL
    pub default_haiku_model:  Option<&'static str>,  // ANTHROPIC_DEFAULT_HAIKU_MODEL
    pub subagent_model:       Option<&'static str>,  // CLAUDE_CODE_SUBAGENT_MODEL

    // ── Long tail: vendor-specific extras ────────────────────────────
    pub extra_env: &'static [(&'static str, &'static str)],
}

/// Per-profile override layer. `None` for any field means "fall through
/// to the [`ProviderSpec`] default". Stored in the `profile` DB row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub base_url:             Option<String>,
    pub model:                Option<String>,
    pub small_fast_model:     Option<String>,
    pub default_opus_model:   Option<String>,
    pub default_sonnet_model: Option<String>,
    pub default_haiku_model:  Option<String>,
    pub subagent_model:       Option<String>,
}

/// Resolved spawn-time env: `(name, value)` pairs to set on the child.
/// Built from a [`ProviderSpec`] + [`Overrides`] + auth_token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEnv {
    pub vars: Vec<(String, String)>,
}
```

- [ ] **Step 1.2: Wire up the submodule + re-exports in `profile/mod.rs`**

Add at the top of `crates/anatta-runtime/src/profile/mod.rs` (after the existing module-level docs and `mod claude; mod codex;` lines around line 14-15):

```rust
pub mod providers;
```

And add to the existing `pub use ...` block (around line 17-18):

```rust
pub use providers::{Overrides, ProviderEnv, ProviderSpec, Tier};
```

- [ ] **Step 1.3: Run `cargo check -p anatta-runtime` to confirm types compile**

Run:
```bash
cargo check -p anatta-runtime
```
Expected: no errors. (`PROVIDERS`, `lookup`, `build` aren't defined yet but no code references them, so the crate compiles.)

- [ ] **Step 1.4: Write the registry-shape tests**

Append to `crates/anatta-runtime/src/profile/providers.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_anthropic() {
        let s = lookup("anthropic").expect("anthropic must exist");
        assert_eq!(s.id, "anthropic");
        assert_eq!(s.backend, "claude");
        assert_eq!(s.tier, Tier::T1);
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("does-not-exist").is_none());
    }

    #[test]
    fn provider_ids_are_unique() {
        let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate provider id in PROVIDERS table");
    }

    #[test]
    fn deepseek_carries_recommended_defaults() {
        let d = lookup("deepseek").unwrap();
        assert_eq!(d.base_url, Some("https://api.deepseek.com/anthropic"));
        assert_eq!(d.model, Some("deepseek-v4-pro"));
        assert_eq!(d.default_opus_model, Some("deepseek-v4-pro"));
        assert_eq!(d.default_sonnet_model, Some("deepseek-v4-pro"));
        assert_eq!(d.default_haiku_model, Some("deepseek-v4-flash"));
        assert_eq!(d.subagent_model, Some("deepseek-v4-flash"));
        assert!(d.extra_env.iter().any(|(k, v)| *k == "CLAUDE_CODE_EFFORT_LEVEL" && *v == "max"));
    }

    #[test]
    fn iter_for_backend_sorts_t1_first() {
        let claude_specs: Vec<&ProviderSpec> = iter_for_backend("claude").collect();
        assert!(!claude_specs.is_empty());
        assert_eq!(claude_specs[0].id, "anthropic", "T1 anthropic must be first");
    }

    #[test]
    fn iter_for_backend_filters_by_backend() {
        for s in iter_for_backend("claude") {
            assert_eq!(s.backend, "claude");
        }
    }
}
```

- [ ] **Step 1.5: Run tests — they MUST fail (no `PROVIDERS` / `lookup` / `iter_for_backend` defined)**

Run:
```bash
cargo test -p anatta-runtime --lib profile::providers
```
Expected: FAIL with "cannot find function `lookup`", etc.

- [ ] **Step 1.6: Implement `PROVIDERS`, `lookup`, `iter_for_backend`**

Append to `crates/anatta-runtime/src/profile/providers.rs` (above the `#[cfg(test)]` block):

```rust
/// Static provider registry. Order = display order in CLI pickers.
/// Tier should be non-decreasing as you go down the list (T1, T2, T3).
pub const PROVIDERS: &[ProviderSpec] = &[
    // ── Tier 1 ──────────────────────────────────────────────────────
    ProviderSpec {
        id:           "anthropic",
        display_name: "Anthropic",
        backend:      "claude",
        tier:         Tier::T1,
        supported_auth: &["login", "api_key"],
        base_url:             None,
        model:                None,
        small_fast_model:     None,
        default_opus_model:   None,
        default_sonnet_model: None,
        default_haiku_model:  None,
        subagent_model:       None,
        extra_env: &[],
    },
    ProviderSpec {
        id:           "openai",
        display_name: "OpenAI",
        backend:      "codex",
        tier:         Tier::T1,
        supported_auth: &["login", "api_key"],
        base_url:             None,
        model:                None,
        small_fast_model:     None,
        default_opus_model:   None,
        default_sonnet_model: None,
        default_haiku_model:  None,
        subagent_model:       None,
        extra_env: &[],
    },
    // ── Tier 2 ──────────────────────────────────────────────────────
    ProviderSpec {
        id:           "deepseek",
        display_name: "DeepSeek",
        backend:      "claude",
        tier:         Tier::T2,
        supported_auth: &["api_key"],
        base_url:             Some("https://api.deepseek.com/anthropic"),
        model:                Some("deepseek-v4-pro"),
        small_fast_model:     None,
        default_opus_model:   Some("deepseek-v4-pro"),
        default_sonnet_model: Some("deepseek-v4-pro"),
        default_haiku_model:  Some("deepseek-v4-flash"),
        subagent_model:       Some("deepseek-v4-flash"),
        extra_env: &[
            ("CLAUDE_CODE_EFFORT_LEVEL", "max"),
        ],
    },
    // ── Tier 3 ──────────────────────────────────────────────────────
    ProviderSpec {
        id:           "kimi",
        display_name: "Kimi (Moonshot)",
        backend:      "claude",
        tier:         Tier::T3,
        supported_auth: &["api_key"],
        base_url:             Some("https://api.moonshot.ai/anthropic"),
        model:                Some("kimi-k2.5"),
        small_fast_model:     None,
        default_opus_model:   Some("kimi-k2.5"),
        default_sonnet_model: Some("kimi-k2.5"),
        default_haiku_model:  Some("kimi-k2.5"),
        subagent_model:       Some("kimi-k2.5"),
        extra_env: &[
            ("ENABLE_TOOL_SEARCH", "false"),
        ],
    },
    ProviderSpec {
        id:           "minimax",
        display_name: "MiniMax",
        backend:      "claude",
        tier:         Tier::T3,
        supported_auth: &["api_key"],
        base_url:             Some("https://api.minimax.io/anthropic"),
        model:                Some("MiniMax-M2.7"),
        small_fast_model:     None,
        default_opus_model:   Some("MiniMax-M2.7"),
        default_sonnet_model: Some("MiniMax-M2.7"),
        default_haiku_model:  Some("MiniMax-M2.7"),
        subagent_model:       None,
        extra_env: &[
            ("API_TIMEOUT_MS", "3000000"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ],
    },
    ProviderSpec {
        id:           "zai",
        display_name: "Z.AI (智谱)",
        backend:      "claude",
        tier:         Tier::T3,
        supported_auth: &["api_key"],
        base_url:             Some("https://api.z.ai/api/anthropic"),
        model:                None,                            // server-side mapping
        small_fast_model:     None,
        default_opus_model:   None,
        default_sonnet_model: None,
        default_haiku_model:  None,
        subagent_model:       None,
        extra_env: &[
            ("API_TIMEOUT_MS", "3000000"),
        ],
    },
    ProviderSpec {
        id:           "custom",
        display_name: "Custom (user-supplied base URL)",
        backend:      "claude",
        tier:         Tier::T3,
        supported_auth: &["api_key"],
        base_url:             None,                             // MUST be overridden on profile
        model:                None,
        small_fast_model:     None,
        default_opus_model:   None,
        default_sonnet_model: None,
        default_haiku_model:  None,
        subagent_model:       None,
        extra_env: &[],
    },
];

/// Look up a provider by id. Returns `None` if no entry matches.
pub fn lookup(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Iterate providers for a given backend, in display order (T1 first).
/// Relies on `PROVIDERS` being declared in non-decreasing tier order.
pub fn iter_for_backend<'a>(backend: &'a str) -> impl Iterator<Item = &'static ProviderSpec> + 'a {
    PROVIDERS.iter().filter(move |p| p.backend == backend)
}
```

- [ ] **Step 1.7: Run tests — registry-shape tests pass**

Run:
```bash
cargo test -p anatta-runtime --lib profile::providers
```
Expected: PASS for `lookup_finds_anthropic`, `lookup_returns_none_for_unknown`, `provider_ids_are_unique`, `deepseek_carries_recommended_defaults`, `iter_for_backend_sorts_t1_first`, `iter_for_backend_filters_by_backend`.

- [ ] **Step 1.8: Write `ProviderEnv::build` tests**

Append inside the existing `#[cfg(test)] mod tests { ... }` block in `providers.rs`:

```rust
    fn vars_to_map(env: &ProviderEnv) -> std::collections::HashMap<&str, &str> {
        env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }

    #[test]
    fn build_uses_spec_defaults_when_no_overrides() {
        let spec = lookup("deepseek").unwrap();
        let env = ProviderEnv::build(spec, &Overrides::default(), "sk-test".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("ANTHROPIC_BASE_URL"), Some(&"https://api.deepseek.com/anthropic"));
        assert_eq!(m.get("ANTHROPIC_AUTH_TOKEN"), Some(&"sk-test"));
        assert_eq!(m.get("ANTHROPIC_MODEL"), Some(&"deepseek-v4-pro"));
        assert_eq!(m.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some(&"deepseek-v4-flash"));
        assert_eq!(m.get("CLAUDE_CODE_SUBAGENT_MODEL"), Some(&"deepseek-v4-flash"));
        assert_eq!(m.get("CLAUDE_CODE_EFFORT_LEVEL"), Some(&"max"));
    }

    #[test]
    fn build_overrides_take_precedence_over_spec_defaults() {
        let spec = lookup("deepseek").unwrap();
        let over = Overrides {
            base_url: Some("https://my.proxy/anthropic".to_owned()),
            model: Some("custom-model".to_owned()),
            ..Default::default()
        };
        let env = ProviderEnv::build(spec, &over, "sk-test".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("ANTHROPIC_BASE_URL"), Some(&"https://my.proxy/anthropic"));
        assert_eq!(m.get("ANTHROPIC_MODEL"), Some(&"custom-model"));
        // Non-overridden field still uses spec default.
        assert_eq!(m.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some(&"deepseek-v4-flash"));
    }

    #[test]
    fn build_omits_env_vars_with_no_default_and_no_override() {
        let spec = lookup("anthropic").unwrap();
        let env = ProviderEnv::build(spec, &Overrides::default(), "sk-ant-x".to_owned());
        let m = vars_to_map(&env);
        // anthropic spec has all None → only AUTH_TOKEN appears.
        assert_eq!(m.get("ANTHROPIC_AUTH_TOKEN"), Some(&"sk-ant-x"));
        assert!(!m.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!m.contains_key("ANTHROPIC_MODEL"));
        assert!(!m.contains_key("ANTHROPIC_DEFAULT_OPUS_MODEL"));
    }

    #[test]
    fn build_includes_extra_env_unconditionally() {
        let spec = lookup("zai").unwrap();
        let env = ProviderEnv::build(spec, &Overrides::default(), "sk-z".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("API_TIMEOUT_MS"), Some(&"3000000"));
    }

    #[test]
    fn build_override_can_only_supply_value_when_spec_lacks_default() {
        // Custom provider has no base_url default — override must supply one.
        let spec = lookup("custom").unwrap();
        let over = Overrides {
            base_url: Some("https://example.com/api".to_owned()),
            ..Default::default()
        };
        let env = ProviderEnv::build(spec, &over, "sk-custom".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("ANTHROPIC_BASE_URL"), Some(&"https://example.com/api"));
    }
```

- [ ] **Step 1.9: Run new tests — they MUST fail (no `build` impl)**

Run:
```bash
cargo test -p anatta-runtime --lib profile::providers::tests::build_
```
Expected: FAIL with "no associated item named `build` found for struct `ProviderEnv`".

- [ ] **Step 1.10: Implement `ProviderEnv::build`**

Append to `crates/anatta-runtime/src/profile/providers.rs` (above the `#[cfg(test)]` block):

```rust
impl ProviderEnv {
    /// Resolve a final env list from `(spec, overrides, auth_token)`.
    /// For each Anthropic-canonical field: prefer `overrides.X`, fall
    /// back to `spec.X`, omit if both are unset. `extra_env` is always
    /// copied verbatim (vendor-specific knobs are not user-overridable
    /// in v1; users that need to disable one can use the `custom`
    /// provider).
    pub fn build(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
        let mut vars: Vec<(String, String)> = Vec::new();

        let pick = |o: &Option<String>, s: Option<&'static str>| -> Option<String> {
            o.clone().or_else(|| s.map(String::from))
        };
        if let Some(v) = pick(&over.base_url, spec.base_url) {
            vars.push(("ANTHROPIC_BASE_URL".into(), v));
        }
        // auth_token is always set when ProviderEnv is built; the OAuth
        // path skips ProviderEnv entirely (claude-cli reads its keychain).
        vars.push(("ANTHROPIC_AUTH_TOKEN".into(), auth_token));
        if let Some(v) = pick(&over.model, spec.model) {
            vars.push(("ANTHROPIC_MODEL".into(), v));
        }
        if let Some(v) = pick(&over.small_fast_model, spec.small_fast_model) {
            vars.push(("ANTHROPIC_SMALL_FAST_MODEL".into(), v));
        }
        if let Some(v) = pick(&over.default_opus_model, spec.default_opus_model) {
            vars.push(("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), v));
        }
        if let Some(v) = pick(&over.default_sonnet_model, spec.default_sonnet_model) {
            vars.push(("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), v));
        }
        if let Some(v) = pick(&over.default_haiku_model, spec.default_haiku_model) {
            vars.push(("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), v));
        }
        if let Some(v) = pick(&over.subagent_model, spec.subagent_model) {
            vars.push(("CLAUDE_CODE_SUBAGENT_MODEL".into(), v));
        }
        for (k, v) in spec.extra_env {
            vars.push(((*k).to_string(), (*v).to_string()));
        }
        Self { vars }
    }
}
```

- [ ] **Step 1.11: Run all providers tests — pass**

Run:
```bash
cargo test -p anatta-runtime --lib profile::providers
```
Expected: PASS for all 11 tests.

- [ ] **Step 1.12: Commit**

```bash
git add crates/anatta-runtime/src/profile/mod.rs \
        crates/anatta-runtime/src/profile/providers.rs
git commit -m "$(cat <<'EOF'
feat(runtime): add provider registry + ProviderEnv builder

Static const table of provider specs (anthropic, openai, deepseek, kimi,
minimax, zai, custom). Each spec declares Anthropic-canonical env-var
defaults as named optional fields; vendor-specific extras live in a flat
`extra_env` slice. `Overrides` is the per-profile override layer;
`ProviderEnv::build` resolves override > spec > unset and produces the
flat env list for spawn.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: DB Migration for Provider Columns

**Files:**
- Create: `crates/anatta-store/migrations/0002_profile_provider.sql`

This task adds the schema. Code changes to `profile.rs` come in Task 3 (the migration is independently committable if we add a temporary schema-only test).

- [ ] **Step 2.1: Write the migration SQL**

Create `crates/anatta-store/migrations/0002_profile_provider.sql`:

```sql
-- Provider routing on profiles.
--
-- `provider` identifies the API endpoint (anthropic, deepseek, kimi, ...).
-- Existing rows are backfilled to the natural default for their backend
-- (claude → anthropic, codex → openai). New inserts MUST supply provider.
--
-- The seven *_override columns let users override individual env-var
-- values that ProviderSpec defaults would otherwise contribute. NULL
-- means "use the spec default". Power users target a `custom` provider
-- when they need a fully bespoke base_url.
--
-- SQLite ALTER TABLE ADD COLUMN is the only ALTER form supported;
-- backfill runs as a follow-up UPDATE.

ALTER TABLE profile ADD COLUMN provider TEXT NOT NULL DEFAULT 'anthropic';
UPDATE profile SET provider = 'openai' WHERE backend = 'codex';

ALTER TABLE profile ADD COLUMN base_url_override             TEXT;
ALTER TABLE profile ADD COLUMN model_override                TEXT;
ALTER TABLE profile ADD COLUMN small_fast_model_override     TEXT;
ALTER TABLE profile ADD COLUMN default_opus_model_override   TEXT;
ALTER TABLE profile ADD COLUMN default_sonnet_model_override TEXT;
ALTER TABLE profile ADD COLUMN default_haiku_model_override  TEXT;
ALTER TABLE profile ADD COLUMN subagent_model_override       TEXT;

CREATE INDEX profile_provider_idx ON profile (provider);
```

- [ ] **Step 2.2: Run only the migrate-runs-clean assertion through existing tests**

Existing tests in `anatta-store/src/profile.rs` open `Store::open_in_memory()` which runs migrations. They will still pass: their queries don't reference the new columns yet (column additions are non-breaking in SQLite).

Run:
```bash
cargo test -p anatta-store
```
Expected: PASS — migrations apply cleanly to a fresh DB and existing tests still pass.

- [ ] **Step 2.3: Commit (just the migration; queries change in next task)**

```bash
git add crates/anatta-store/migrations/0002_profile_provider.sql
git commit -m "$(cat <<'EOF'
feat(store): migration adds provider + 7 override columns to profile

provider is NOT NULL (default 'anthropic'); existing codex rows backfilled
to 'openai'. The seven *_override columns are nullable — NULL means "use
the ProviderSpec default for this field". Index on provider for filtered
listings.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Store Profile API — Plumb Provider + Overrides

**Files:**
- Modify: `crates/anatta-store/src/profile.rs`

This task threads the new columns through `ProfileRecord`, `NewProfile`, and the three queries (`insert_profile`, `list_profiles`, `get_profile`). Then regenerates the sqlx offline cache.

- [ ] **Step 3.1: Add a failing round-trip test for the new fields**

Append to the `#[cfg(test)] mod tests { ... }` block in `crates/anatta-store/src/profile.rs`:

```rust
    #[tokio::test]
    async fn round_trip_with_provider_and_overrides() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-DSeek01",
            backend: BackendKind::Claude,
            name: "ds",
            auth_method: AuthMethod::ApiKey,
            provider: "deepseek",
            base_url_override: None,
            model_override: Some("deepseek-v4-pro"),
            small_fast_model_override: None,
            default_opus_model_override: None,
            default_sonnet_model_override: None,
            default_haiku_model_override: Some("deepseek-v4-flash"),
            subagent_model_override: None,
        })
        .await
        .unwrap();

        let got = s.get_profile("claude-DSeek01").await.unwrap().unwrap();
        assert_eq!(got.provider, "deepseek");
        assert_eq!(got.model_override.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(got.default_haiku_model_override.as_deref(), Some("deepseek-v4-flash"));
        assert!(got.base_url_override.is_none());
        assert!(got.subagent_model_override.is_none());
    }

    #[tokio::test]
    async fn legacy_default_provider_for_inserts() {
        // Mimic old code path: API forces caller to supply provider, so
        // there is no default-from-DB path tested via Rust API. Instead
        // we just confirm a claude profile can be inserted with provider=anthropic.
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-Anth001",
            backend: BackendKind::Claude,
            name: "default",
            auth_method: AuthMethod::Login,
            provider: "anthropic",
            base_url_override: None,
            model_override: None,
            small_fast_model_override: None,
            default_opus_model_override: None,
            default_sonnet_model_override: None,
            default_haiku_model_override: None,
            subagent_model_override: None,
        })
        .await
        .unwrap();
        let got = s.get_profile("claude-Anth001").await.unwrap().unwrap();
        assert_eq!(got.provider, "anthropic");
    }
```

- [ ] **Step 3.2: Run tests — MUST fail (compile error: missing fields on `NewProfile` / `ProfileRecord`)**

Run:
```bash
cargo test -p anatta-store --lib profile
```
Expected: compile FAIL with "missing fields `provider`, ..." in `NewProfile { ... }`.

- [ ] **Step 3.3: Extend `ProfileRecord`, `NewProfile`, `ProfileRow`**

Edit `crates/anatta-store/src/profile.rs` — replace the `ProfileRecord`, `NewProfile`, `ProfileRow` structs (around lines 53-95) with:

```rust
/// Public typed view of one row in the `profile` table.
#[derive(Debug, Clone)]
pub struct ProfileRecord {
    pub id: String,
    pub backend: BackendKind,
    pub name: String,
    pub auth_method: AuthMethod,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,

    pub provider: String,
    pub base_url_override:             Option<String>,
    pub model_override:                Option<String>,
    pub small_fast_model_override:     Option<String>,
    pub default_opus_model_override:   Option<String>,
    pub default_sonnet_model_override: Option<String>,
    pub default_haiku_model_override:  Option<String>,
    pub subagent_model_override:       Option<String>,
}

/// What the caller passes to insert a new row.
#[derive(Debug, Clone)]
pub struct NewProfile<'a> {
    pub id: &'a str,
    pub backend: BackendKind,
    pub name: &'a str,
    pub auth_method: AuthMethod,

    pub provider: &'a str,
    pub base_url_override:             Option<&'a str>,
    pub model_override:                Option<&'a str>,
    pub small_fast_model_override:     Option<&'a str>,
    pub default_opus_model_override:   Option<&'a str>,
    pub default_sonnet_model_override: Option<&'a str>,
    pub default_haiku_model_override:  Option<&'a str>,
    pub subagent_model_override:       Option<&'a str>,
}

/// Internal flat row, populated directly by `sqlx::query_as!`.
struct ProfileRow {
    id: String,
    backend: String,
    name: String,
    auth_method: String,
    created_at: String,
    last_used_at: Option<String>,
    provider: String,
    base_url_override:             Option<String>,
    model_override:                Option<String>,
    small_fast_model_override:     Option<String>,
    default_opus_model_override:   Option<String>,
    default_sonnet_model_override: Option<String>,
    default_haiku_model_override:  Option<String>,
    subagent_model_override:       Option<String>,
}

impl ProfileRow {
    fn into_record(self) -> Result<ProfileRecord, StoreError> {
        Ok(ProfileRecord {
            backend: BackendKind::parse(&self.backend)?,
            auth_method: AuthMethod::parse(&self.auth_method)?,
            created_at: parse_ts(&self.created_at)?,
            last_used_at: self.last_used_at.map(|s| parse_ts(&s)).transpose()?,
            id: self.id,
            name: self.name,
            provider: self.provider,
            base_url_override:             self.base_url_override,
            model_override:                self.model_override,
            small_fast_model_override:     self.small_fast_model_override,
            default_opus_model_override:   self.default_opus_model_override,
            default_sonnet_model_override: self.default_sonnet_model_override,
            default_haiku_model_override:  self.default_haiku_model_override,
            subagent_model_override:       self.subagent_model_override,
        })
    }
}
```

- [ ] **Step 3.4: Update `insert_profile` query**

Replace the `insert_profile` body in `crates/anatta-store/src/profile.rs` with:

```rust
    /// Insert a new profile. Fails on `(backend, name)` collision.
    pub async fn insert_profile(&self, p: NewProfile<'_>) -> Result<(), StoreError> {
        let backend = p.backend.as_str();
        let auth = p.auth_method.as_str();
        let now = Utc::now().to_rfc3339();
        sqlx::query!(
            r#"
            INSERT INTO profile (
                id, backend, name, auth_method, created_at, last_used_at,
                provider,
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override
            )
            VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            p.id,
            backend,
            p.name,
            auth,
            now,
            p.provider,
            p.base_url_override,
            p.model_override,
            p.small_fast_model_override,
            p.default_opus_model_override,
            p.default_sonnet_model_override,
            p.default_haiku_model_override,
            p.subagent_model_override,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 3.5: Update `list_profiles` and `get_profile` queries**

Replace both query bodies. The SELECT list grows to include the 8 new columns.

```rust
    pub async fn list_profiles(&self) -> Result<Vec<ProfileRecord>, StoreError> {
        let rows = sqlx::query_as!(
            ProfileRow,
            r#"
            SELECT
                id           AS "id!",
                backend      AS "backend!",
                name         AS "name!",
                auth_method  AS "auth_method!",
                created_at   AS "created_at!",
                last_used_at,
                provider     AS "provider!",
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override
            FROM profile
            ORDER BY COALESCE(last_used_at, created_at) DESC
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ProfileRow::into_record).collect()
    }

    pub async fn get_profile(&self, id: &str) -> Result<Option<ProfileRecord>, StoreError> {
        let row = sqlx::query_as!(
            ProfileRow,
            r#"
            SELECT
                id           AS "id!",
                backend      AS "backend!",
                name         AS "name!",
                auth_method  AS "auth_method!",
                created_at   AS "created_at!",
                last_used_at,
                provider     AS "provider!",
                base_url_override,
                model_override,
                small_fast_model_override,
                default_opus_model_override,
                default_sonnet_model_override,
                default_haiku_model_override,
                subagent_model_override
            FROM profile
            WHERE id = ?
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(ProfileRow::into_record).transpose()
    }
```

- [ ] **Step 3.6: Update existing tests to populate the new fields**

Existing tests at lines ~196-291 build `NewProfile` literals. They MUST be extended with the new fields (all `None` / `"anthropic"` / `"openai"`). For each `NewProfile { ... }` literal in the test module:

- Add `provider: "anthropic"` (or `"openai"` for codex rows).
- Add the 7 override fields, all `None`.

Concrete example — the existing `insert_and_get_round_trip` becomes:

```rust
    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let s = store().await;
        s.insert_profile(NewProfile {
            id: "claude-AbCd1234",
            backend: BackendKind::Claude,
            name: "work",
            auth_method: AuthMethod::Login,
            provider: "anthropic",
            base_url_override: None,
            model_override: None,
            small_fast_model_override: None,
            default_opus_model_override: None,
            default_sonnet_model_override: None,
            default_haiku_model_override: None,
            subagent_model_override: None,
        })
        .await
        .unwrap();

        let got = s.get_profile("claude-AbCd1234").await.unwrap().unwrap();
        assert_eq!(got.id, "claude-AbCd1234");
        assert_eq!(got.backend, BackendKind::Claude);
        assert_eq!(got.name, "work");
        assert_eq!(got.auth_method, AuthMethod::Login);
        assert_eq!(got.provider, "anthropic");
        assert!(got.last_used_at.is_none());
    }
```

Apply the same expansion to: `list_orders_by_recency` (both inserts), `unique_constraint_on_backend_name` (both inserts), `delete_returns_true_only_when_row_existed`, `get_missing_returns_none` (no insert needed). The codex insert in `list_orders_by_recency` uses `provider: "openai"`.

- [ ] **Step 3.7: Regenerate the sqlx offline cache**

Required because we modified `query!` / `query_as!` macros. Without this, `cargo build` fails under SQLX_OFFLINE=true.

Run (from repo root):
```bash
./scripts/sqlx-prepare.sh
```
Expected:
```
regenerated /.../crates/anatta-store/.sqlx/ — commit changes.
```

If the script fails because sqlx-cli is missing, install it first:
```bash
cargo install sqlx-cli --no-default-features --features sqlite,rustls
```

- [ ] **Step 3.8: Run all anatta-store tests**

Run:
```bash
cargo test -p anatta-store
```
Expected: all PASS, including the two new round-trip tests.

- [ ] **Step 3.9: Commit**

```bash
git add crates/anatta-store/src/profile.rs \
        crates/anatta-store/.sqlx/
git commit -m "$(cat <<'EOF'
feat(store): expose provider + 7 override columns on ProfileRecord

ProfileRecord, NewProfile, and the three queries now thread provider +
seven nullable *_override fields through. Existing tests updated to
supply the new fields explicitly. sqlx offline cache regenerated.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: ClaudeLaunch ProviderEnv Injection

**Files:**
- Modify: `crates/anatta-runtime/src/spawn/claude.rs`
- Modify: `crates/anatta-runtime/tests/spawn_mock.rs`

This task adds `provider: Option<ProviderEnv>` to `ClaudeLaunch` and injects its vars into the child process. CodexLaunch is unchanged — codex doesn't have a multi-provider story yet.

- [ ] **Step 4.1: Write a failing integration test that asserts env injection**

Append to `crates/anatta-runtime/tests/spawn_mock.rs` (use any existing `write_mock_script` helper — see line 22 onwards in the current file):

```rust
#[tokio::test]
async fn launch_injects_provider_env_into_child_process() {
    use anatta_runtime::profile::ProviderEnv;

    let tmp = tempfile::tempdir().unwrap();
    let anatta_root = tmp.path();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), anatta_root).unwrap();

    // Mock script: dump env to $CLAUDE_CONFIG_DIR/env.dump, then emit a
    // single system/init line so the launch resolves successfully.
    let dir = tmp.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("claude-mock.sh");
    let script = r#"#!/bin/sh
env > "$CLAUDE_CONFIG_DIR/env.dump"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake-uuid","cwd":"/tmp","model":"x","tools":[],"mcp_servers":[],"permission_mode":"default","slash_commands":[]}'
"#;
    std::fs::write(&script_path, script).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let provider = ProviderEnv {
        vars: vec![
            ("ANTHROPIC_BASE_URL".into(), "https://api.test.example/anthropic".into()),
            ("ANTHROPIC_AUTH_TOKEN".into(), "sk-test-abc".into()),
            ("ANTHROPIC_MODEL".into(), "test-model-7b".into()),
            ("CLAUDE_CODE_EFFORT_LEVEL".into(), "max".into()),
        ],
    };

    let session = ClaudeLaunch {
        profile: profile.clone(),
        cwd: tmp.path().to_owned(),
        prompt: "hi".into(),
        resume: None,
        binary_path: script_path,
        provider: Some(provider),
    }
    .launch()
    .await
    .expect("launch should succeed against mock script");

    // Wait for child to exit so env.dump is fully flushed.
    let _ = session.wait().await.unwrap();

    let dumped = std::fs::read_to_string(profile.path.join("env.dump"))
        .expect("env dump should exist");
    assert!(dumped.contains("ANTHROPIC_BASE_URL=https://api.test.example/anthropic"),
        "missing ANTHROPIC_BASE_URL in dump:\n{dumped}");
    assert!(dumped.contains("ANTHROPIC_AUTH_TOKEN=sk-test-abc"));
    assert!(dumped.contains("ANTHROPIC_MODEL=test-model-7b"));
    assert!(dumped.contains("CLAUDE_CODE_EFFORT_LEVEL=max"));
}

#[tokio::test]
async fn launch_without_provider_does_not_inject_anthropic_env() {
    let tmp = tempfile::tempdir().unwrap();
    let anatta_root = tmp.path();
    let profile = ClaudeProfile::create(ClaudeProfileId::new(), anatta_root).unwrap();

    let dir = tmp.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("claude-mock.sh");
    let script = r#"#!/bin/sh
env > "$CLAUDE_CONFIG_DIR/env.dump"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake-uuid","cwd":"/tmp","model":"x","tools":[],"mcp_servers":[],"permission_mode":"default","slash_commands":[]}'
"#;
    std::fs::write(&script_path, script).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let session = ClaudeLaunch {
        profile: profile.clone(),
        cwd: tmp.path().to_owned(),
        prompt: "hi".into(),
        resume: None,
        binary_path: script_path,
        provider: None,
    }
    .launch()
    .await
    .expect("launch");
    let _ = session.wait().await.unwrap();

    let dumped = std::fs::read_to_string(profile.path.join("env.dump")).unwrap();
    // CLAUDE_CONFIG_DIR is always set by the spawn code; we assert that
    // ANTHROPIC_AUTH_TOKEN is NOT — that's the OAuth path.
    assert!(dumped.contains("CLAUDE_CONFIG_DIR="), "CLAUDE_CONFIG_DIR should always be set");
    assert!(!dumped.contains("ANTHROPIC_AUTH_TOKEN="),
        "ANTHROPIC_AUTH_TOKEN must NOT be set in OAuth path:\n{dumped}");
    assert!(!dumped.contains("ANTHROPIC_BASE_URL="),
        "ANTHROPIC_BASE_URL must NOT be set in OAuth path");
}
```

- [ ] **Step 4.2: Run new tests — MUST fail (no `provider` field on `ClaudeLaunch`)**

Run:
```bash
cargo test -p anatta-runtime --features spawn --test spawn_mock launch_injects launch_without_provider
```
Expected: compile FAIL with "struct `ClaudeLaunch` has no field named `provider`".

- [ ] **Step 4.3: Add the `provider` field to `ClaudeLaunch` and inject in `launch()`**

Edit `crates/anatta-runtime/src/spawn/claude.rs`. Update the struct definition (around lines 17-29):

```rust
/// Configuration for spawning a claude session.
#[derive(Debug, Clone)]
pub struct ClaudeLaunch {
    pub profile: ClaudeProfile,
    pub cwd: PathBuf,
    pub prompt: String,
    /// `Some(id)` → launch with `--resume <id>` to continue an existing
    /// session. `None` → start fresh.
    pub resume: Option<ClaudeSessionId>,
    /// Path to the claude binary. Use [`crate::distribution::install`]
    /// (with the `installer` feature) to obtain it under anatta-managed
    /// paths.
    pub binary_path: PathBuf,
    /// Provider routing. `Some(env)` injects ANTHROPIC_BASE_URL / AUTH_TOKEN /
    /// MODEL / vendor extras into the child. `None` = use claude-cli's own
    /// auth + endpoint (OAuth keychain path).
    pub provider: Option<crate::profile::ProviderEnv>,
}
```

In the `launch()` body (around lines 41-43), inject after the existing `cmd.env("CLAUDE_CONFIG_DIR", ...)` line:

```rust
        cmd.env("CLAUDE_CONFIG_DIR", &self.profile.path);
        if let Some(env) = &self.provider {
            for (k, v) in &env.vars {
                cmd.env(k, v);
            }
        }
        cmd.current_dir(&self.cwd);
```

- [ ] **Step 4.4: Run new spawn tests — MUST pass**

Run:
```bash
cargo test -p anatta-runtime --features spawn --test spawn_mock launch_injects launch_without_provider
```
Expected: PASS.

- [ ] **Step 4.5: Run the full anatta-runtime test suite to confirm no regressions**

Run:
```bash
cargo test -p anatta-runtime --features spawn --features installer
```
Expected: all PASS. Existing `launch_extracts_session_id_from_first_init_event` and other spawn_mock tests must still pass — but they pre-date this change and don't supply the new field. Update them.

If existing tests fail to compile, they need `provider: None` added to their `ClaudeLaunch { ... }` literals in `crates/anatta-runtime/tests/spawn_mock.rs` and `crates/anatta-runtime/tests/spawn_e2e.rs`. Apply that edit, re-run.

- [ ] **Step 4.6: Commit**

```bash
git add crates/anatta-runtime/src/spawn/claude.rs \
        crates/anatta-runtime/tests/spawn_mock.rs \
        crates/anatta-runtime/tests/spawn_e2e.rs
git commit -m "$(cat <<'EOF'
feat(runtime): inject ProviderEnv vars into claude child process

ClaudeLaunch grows a `provider: Option<ProviderEnv>` field. When set,
each (k, v) is forwarded to the child via Command::env. When None,
claude-cli's own OAuth/keychain path is used unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: CLI — Provider Selection, Flags, Login Guard, `show`/`list` Updates

**Files:**
- Modify: `apps/anatta-cli/src/profile.rs`

This task is the user-facing wiring. After this, `anatta profile create` lets the user pick provider, supplies overrides via flags, and rejects login auth for providers that don't support it. `show` and `list` surface the provider/model.

The plan groups several related edits into one task because they're tightly coupled (the CLI structure changes in lockstep). Commits inside the task are still encouraged at milestones (after add-flags, after interactive picker, after show/list).

- [ ] **Step 5.1: Extend `ProfileCommand::Create` with provider + override flags**

Edit `apps/anatta-cli/src/profile.rs`. Update the `Create` variant (around lines 17-30):

```rust
    /// Create a new profile (interactive by default; --non-interactive uses flags only).
    Create {
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        auth: Option<AuthArg>,
        /// Provide the API key inline (only with --auth api-key).
        #[arg(long, env = "ANATTA_PROFILE_API_KEY")]
        api_key: Option<String>,
        /// Fail if any required input is missing instead of prompting.
        #[arg(long)]
        non_interactive: bool,

        /// Provider id (e.g. `anthropic`, `deepseek`, `kimi`, `minimax`,
        /// `zai`, `custom`). Defaults to `anthropic` for claude / `openai`
        /// for codex when omitted.
        #[arg(long)]
        provider: Option<String>,
        /// Override `ANTHROPIC_BASE_URL` (required for `--provider custom`).
        #[arg(long)]
        base_url: Option<String>,
        /// Override `ANTHROPIC_MODEL`.
        #[arg(long)]
        model: Option<String>,
        /// Override `ANTHROPIC_SMALL_FAST_MODEL`.
        #[arg(long)]
        small_fast_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_OPUS_MODEL`.
        #[arg(long)]
        opus_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_SONNET_MODEL`.
        #[arg(long)]
        sonnet_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_HAIKU_MODEL`.
        #[arg(long)]
        haiku_model: Option<String>,
        /// Override `CLAUDE_CODE_SUBAGENT_MODEL`.
        #[arg(long)]
        subagent_model: Option<String>,
    },
```

Update `pub async fn run(...)` and `create(...)` to accept and pass these new fields. The `run()` match arm becomes:

```rust
        ProfileCommand::Create {
            backend,
            name,
            auth,
            api_key,
            non_interactive,
            provider,
            base_url,
            model,
            small_fast_model,
            opus_model,
            sonnet_model,
            haiku_model,
            subagent_model,
        } => {
            create(
                cfg, backend, name, auth, api_key, non_interactive,
                provider, base_url, model, small_fast_model,
                opus_model, sonnet_model, haiku_model, subagent_model,
            )
            .await
        }
```

- [ ] **Step 5.2: Update `create()` signature and add provider resolution**

Replace the `create` function (lines ~120-246). Below is the full replacement; key additions are commented inline.

```rust
async fn create(
    cfg: &Config,
    backend_flag: Option<BackendArg>,
    name_flag: Option<String>,
    auth_flag: Option<AuthArg>,
    api_key_flag: Option<String>,
    non_interactive: bool,
    provider_flag: Option<String>,
    base_url_flag: Option<String>,
    model_flag: Option<String>,
    small_fast_model_flag: Option<String>,
    opus_model_flag: Option<String>,
    sonnet_model_flag: Option<String>,
    haiku_model_flag: Option<String>,
    subagent_model_flag: Option<String>,
) -> Result<(), ProfileCmdError> {
    let theme = ColorfulTheme::default();

    // 1. backend
    let backend: BackendKind = match backend_flag {
        Some(b) => b.into(),
        None => {
            if non_interactive {
                return Err(ProfileCmdError::InputRequired("backend"));
            }
            let items = ["claude", "codex"];
            let pick = dialoguer::Select::with_theme(&theme)
                .with_prompt("Backend")
                .default(0)
                .items(&items)
                .interact()?;
            if pick == 0 { BackendKind::Claude } else { BackendKind::Codex }
        }
    };
    let backend_str = backend.as_str();

    // 2. provider (default depends on backend)
    let provider_id: String = match provider_flag {
        Some(p) => p,
        None => {
            if non_interactive {
                // Non-interactive default: anthropic / openai.
                match backend {
                    BackendKind::Claude => "anthropic".to_owned(),
                    BackendKind::Codex => "openai".to_owned(),
                }
            } else {
                let candidates: Vec<&'static anatta_runtime::profile::ProviderSpec> =
                    anatta_runtime::profile::providers::iter_for_backend(backend_str).collect();
                let labels: Vec<String> = candidates
                    .iter()
                    .map(|s| format!("{}  ({:?}, {})", s.display_name, s.tier,
                                     s.supported_auth.join("+")))
                    .collect();
                let pick = dialoguer::Select::with_theme(&theme)
                    .with_prompt("Provider")
                    .default(0)
                    .items(&labels)
                    .interact()?;
                candidates[pick].id.to_owned()
            }
        }
    };
    let spec = anatta_runtime::profile::providers::lookup(&provider_id)
        .ok_or_else(|| ProfileCmdError::UnknownProvider(provider_id.clone()))?;
    if spec.backend != backend_str {
        return Err(ProfileCmdError::ProviderBackendMismatch {
            provider: provider_id.clone(),
            expected: backend_str,
            got: spec.backend,
        });
    }

    // 3. name
    let name: String = match name_flag {
        Some(n) => n,
        None => {
            if non_interactive {
                return Err(ProfileCmdError::InputRequired("name"));
            }
            dialoguer::Input::<String>::with_theme(&theme)
                .with_prompt("Name (label, e.g. work / personal)")
                .interact_text()?
        }
    };

    // 4. auth method (constrained by spec.supported_auth)
    let auth_method: AuthMethod = match auth_flag {
        Some(a) => {
            let am: AuthMethod = a.into();
            if !spec.supported_auth.contains(&am.as_str()) {
                return Err(ProfileCmdError::AuthNotSupportedByProvider {
                    provider: provider_id.clone(),
                    auth: am.as_str(),
                });
            }
            am
        }
        None => {
            // Auto-pick when only one option; prompt otherwise.
            if spec.supported_auth == ["api_key"] {
                AuthMethod::ApiKey
            } else if non_interactive {
                return Err(ProfileCmdError::InputRequired("auth"));
            } else {
                let items: Vec<&str> = spec.supported_auth.iter().copied().collect();
                let pick = dialoguer::Select::with_theme(&theme)
                    .with_prompt("Auth method")
                    .default(0)
                    .items(&items)
                    .interact()?;
                AuthMethod::parse(items[pick])
                    .map_err(|_| ProfileCmdError::InputRequired("auth"))?
            }
        }
    };

    // 5. (if api-key) gather the key
    let api_key: Option<String> = if matches!(auth_method, AuthMethod::ApiKey) {
        match api_key_flag {
            Some(k) => Some(k),
            None => {
                if non_interactive {
                    return Err(ProfileCmdError::InputRequired("api-key"));
                }
                Some(
                    dialoguer::Password::with_theme(&theme)
                        .with_prompt("API key")
                        .interact()?,
                )
            }
        }
    } else {
        None
    };

    // 6. (custom provider) require a base_url
    if provider_id == "custom" && base_url_flag.is_none() {
        return Err(ProfileCmdError::InputRequired("base-url"));
    }

    // 7. mint id, create on-disk profile
    let (profile_path, id_string): (std::path::PathBuf, String) = match backend {
        BackendKind::Claude => {
            let id = ClaudeProfileId::new();
            let p = ClaudeProfile::create(id.clone(), &cfg.anatta_home)?;
            (p.path, id.as_str().to_owned())
        }
        BackendKind::Codex => {
            let id = CodexProfileId::new();
            let p = CodexProfile::create(id.clone(), &cfg.anatta_home)?;
            (p.path, id.as_str().to_owned())
        }
    };

    println!(
        "→ Generated id: {}\n→ Provider: {}\n→ Profile dir: {}",
        id_string,
        provider_id,
        profile_path.display()
    );

    // 8. run auth (with rollback on failure)
    let outcome = run_auth(backend, &profile_path, &id_string, auth_method, api_key.as_deref()).await;
    if let Err(e) = outcome {
        let _ = std::fs::remove_dir_all(&profile_path);
        let _ = auth::delete_api_key(&id_string);
        return Err(ProfileCmdError::RolledBack {
            source: Box::new(e),
        });
    }

    // 9. commit DB row
    if let Err(e) = cfg
        .store
        .insert_profile(NewProfile {
            id: &id_string,
            backend,
            name: &name,
            auth_method,
            provider: &provider_id,
            base_url_override: base_url_flag.as_deref(),
            model_override: model_flag.as_deref(),
            small_fast_model_override: small_fast_model_flag.as_deref(),
            default_opus_model_override: opus_model_flag.as_deref(),
            default_sonnet_model_override: sonnet_model_flag.as_deref(),
            default_haiku_model_override: haiku_model_flag.as_deref(),
            subagent_model_override: subagent_model_flag.as_deref(),
        })
        .await
    {
        let _ = std::fs::remove_dir_all(&profile_path);
        let _ = auth::delete_api_key(&id_string);
        return Err(e.into());
    }

    println!("✓ {id_string} (\"{name}\") created.");
    Ok(())
}
```

- [ ] **Step 5.3: Add the new error variants**

Update `ProfileCmdError` (around lines 73-97). Add three variants:

```rust
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("provider {provider} targets backend {got}, but profile uses backend {expected}")]
    ProviderBackendMismatch {
        provider: String,
        expected: &'static str,
        got: &'static str,
    },
    #[error("provider {provider} does not support auth method {auth}")]
    AuthNotSupportedByProvider {
        provider: String,
        auth: &'static str,
    },
```

- [ ] **Step 5.4: Update `show` to surface provider + override fields**

Replace the `show` function body (around lines 306-340) with:

```rust
async fn show(cfg: &Config, id: &str) -> Result<(), ProfileCmdError> {
    let r = cfg
        .store
        .get_profile(id)
        .await?
        .ok_or_else(|| ProfileCmdError::ProfileNotFound(id.to_owned()))?;
    println!("{:<22} {}", "id:", r.id);
    println!("{:<22} {}", "backend:", r.backend.as_str());
    println!("{:<22} {}", "name:", r.name);
    println!("{:<22} {}", "provider:", r.provider);
    println!("{:<22} {}", "auth_method:", r.auth_method.as_str());
    println!("{:<22} {}", "created_at:", r.created_at);
    println!(
        "{:<22} {}",
        "last_used_at:",
        r.last_used_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(never)".into())
    );
    let any = AnyProfileId::parse(&r.id)
        .map_err(|e| ProfileCmdError::BadId(format!("{e}")))?;
    let dir = cfg.anatta_home.join("profiles").join(any.as_str());
    println!("{:<22} {}", "path:", dir.display());

    // Overrides — only print non-None ones.
    let overrides: &[(&str, Option<&str>)] = &[
        ("base_url:",       r.base_url_override.as_deref()),
        ("model:",          r.model_override.as_deref()),
        ("small_fast_model:", r.small_fast_model_override.as_deref()),
        ("opus_model:",     r.default_opus_model_override.as_deref()),
        ("sonnet_model:",   r.default_sonnet_model_override.as_deref()),
        ("haiku_model:",    r.default_haiku_model_override.as_deref()),
        ("subagent_model:", r.subagent_model_override.as_deref()),
    ];
    let any_override = overrides.iter().any(|(_, v)| v.is_some());
    if any_override {
        println!("{:<22}", "overrides:");
        for (label, val) in overrides {
            if let Some(v) = val {
                println!("  {:<20} {}", label, v);
            }
        }
    }

    if matches!(r.auth_method, AuthMethod::ApiKey) {
        let has = auth::read_api_key(&r.id)?.is_some();
        println!(
            "{:<22} {}",
            "api_key:",
            if has { "(in keyring)" } else { "(missing)" }
        );
    }
    Ok(())
}
```

- [ ] **Step 5.5: Update `list` to include the provider column**

Replace the `list` function (around lines 279-304) with:

```rust
async fn list(cfg: &Config) -> Result<(), ProfileCmdError> {
    let rows = cfg.store.list_profiles().await?;
    if rows.is_empty() {
        println!("(no profiles yet — `anatta profile create` to add one)");
        return Ok(());
    }
    println!(
        "{:<24}  {:<8}  {:<10}  {:<16}  {:<8}  {}",
        "ID", "BACKEND", "PROVIDER", "NAME", "AUTH", "LAST USED"
    );
    for r in rows {
        let last = r
            .last_used_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "never".into());
        println!(
            "{:<24}  {:<8}  {:<10}  {:<16}  {:<8}  {}",
            r.id,
            r.backend.as_str(),
            r.provider,
            r.name,
            r.auth_method.as_str(),
            last
        );
    }
    Ok(())
}
```

- [ ] **Step 5.6: Build the CLI to make sure everything compiles**

Run:
```bash
cargo build -p anatta-cli
```
Expected: clean build.

- [ ] **Step 5.7: Smoke-test the non-interactive create path**

Run:
```bash
ANATTA_HOME="$(mktemp -d)" \
  cargo run -p anatta-cli -- profile create \
    --non-interactive \
    --backend claude \
    --provider deepseek \
    --name dev \
    --auth api-key \
    --api-key sk-fake-test \
    --model deepseek-v4-pro
```
Expected: prints `✓ claude-XXXXXXXX ("dev") created.` and the profile dir is created in the temp `ANATTA_HOME`.

Then:
```bash
ANATTA_HOME=<the_same_temp_dir> cargo run -p anatta-cli -- profile list
```
Expected: lists the new profile with `BACKEND=claude PROVIDER=deepseek`.

```bash
ANATTA_HOME=<the_same_temp_dir> cargo run -p anatta-cli -- profile show claude-XXXXXXXX
```
Expected: prints `provider: deepseek`, `model: deepseek-v4-pro` under overrides, `api_key: (in keyring)`.

Note: macOS will store the test API key in keyring. Delete it after testing:
```bash
ANATTA_HOME=<the_same_temp_dir> cargo run -p anatta-cli -- profile delete claude-XXXXXXXX --yes
```

- [ ] **Step 5.8: Smoke-test the login-guard path**

```bash
ANATTA_HOME="$(mktemp -d)" \
  cargo run -p anatta-cli -- profile create \
    --non-interactive \
    --backend claude \
    --provider deepseek \
    --name dev \
    --auth login \
    --api-key irrelevant
```
Expected: error like `provider deepseek does not support auth method login` (exit code 1).

- [ ] **Step 5.9: Commit**

```bash
git add apps/anatta-cli/src/profile.rs
git commit -m "$(cat <<'EOF'
feat(cli): provider selection + override flags + login guard

profile create grows --provider, --base-url, --model, --small-fast-model,
--opus-model, --sonnet-model, --haiku-model, --subagent-model. Interactive
mode picks provider from the registry filtered by backend. Auth method is
constrained by the provider's supported_auth (login is rejected for
third-party providers since they only accept API keys). profile show
surfaces provider + non-None overrides. profile list adds a PROVIDER
column.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Workspace Verification

**Files:** None modified — this is a sanity sweep.

- [ ] **Step 6.1: Build entire workspace clean**

Run:
```bash
cargo build --workspace --all-features
```
Expected: clean build, no warnings.

- [ ] **Step 6.2: Run the full test suite**

Run:
```bash
cargo test --workspace --all-features
```
Expected: all PASS, including the new provider tests, store round-trip tests, and spawn env-injection tests.

- [ ] **Step 6.3: Clippy with warnings-as-errors**

Run:
```bash
cargo clippy --workspace --all-features -- -D warnings
```
Expected: clean.

- [ ] **Step 6.4: Verify new sqlx cache entries are tracked in git**

Run:
```bash
git status crates/anatta-store/.sqlx/
```
Expected: clean (everything from Task 3 was committed). If there are unstaged JSON files, something escaped from Task 3 — re-run sqlx-prepare and amend.

- [ ] **Step 6.5: (Optional) End-to-end: create a profile, dump the DB**

```bash
ANATTA_HOME=/tmp/anatta-final-check
rm -rf "$ANATTA_HOME"
cargo run -p anatta-cli -- profile create \
  --non-interactive --backend claude --provider zai --name z \
  --auth api-key --api-key sk-fake-zai
sqlite3 "$ANATTA_HOME/anatta.db" "SELECT id, backend, provider, name FROM profile;"
```
Expected: one row with `provider=zai`.

Clean up:
```bash
cargo run -p anatta-cli --anatta-home "$ANATTA_HOME" -- profile delete <id> --yes
rm -rf "$ANATTA_HOME"
```

---

## Notes for the Executor

- `sqlx-cli` (one-time install) is required for the migration step. If `./scripts/sqlx-prepare.sh` errors out with `sqlx-cli not found`, install via `cargo install sqlx-cli --no-default-features --features sqlite,rustls`.
- The two existing `spawn_mock` integration tests (`launch_extracts_session_id_from_first_init_event` and any others using `ClaudeLaunch { ... }` literals) need `provider: None` added to compile. Same for `spawn_e2e.rs`.
- macOS keyring entries from smoke testing accumulate under service `"anatta"`. The `profile delete` command removes them; if a smoke test errors out before delete, clean up manually with `security delete-generic-password -s anatta -a <profile-id>`.
- For `provider_id == "custom"` we require `--base-url` at create time. We do NOT validate the URL — that's claude-cli's job at spawn time.
- Codex backend currently always uses `provider="openai"` automatically. Codex-specific spawn injection (analogous to Task 4) is out of scope here; revisit when a non-OpenAI codex provider appears.
