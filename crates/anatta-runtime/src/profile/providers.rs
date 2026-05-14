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
    pub id: &'static str,           // "anthropic", "deepseek", ...
    pub display_name: &'static str, // "Anthropic", "DeepSeek", ...
    pub backend: &'static str,      // "claude" / "codex"
    pub tier: Tier,
    /// Auth methods supported by this provider. CLI rejects any choice
    /// not in this list. Currently "login" / "api_key".
    pub supported_auth: &'static [&'static str],

    // ── Anthropic-canonical env vars (None = don't set) ──────────────
    pub base_url: Option<&'static str>, // ANTHROPIC_BASE_URL
    pub model: Option<&'static str>,    // ANTHROPIC_MODEL
    pub small_fast_model: Option<&'static str>, // ANTHROPIC_SMALL_FAST_MODEL
    pub default_opus_model: Option<&'static str>, // ANTHROPIC_DEFAULT_OPUS_MODEL
    pub default_sonnet_model: Option<&'static str>, // ANTHROPIC_DEFAULT_SONNET_MODEL
    pub default_haiku_model: Option<&'static str>, // ANTHROPIC_DEFAULT_HAIKU_MODEL
    pub subagent_model: Option<&'static str>, // CLAUDE_CODE_SUBAGENT_MODEL

    // ── Long tail: vendor-specific extras ────────────────────────────
    pub extra_env: &'static [(&'static str, &'static str)],
}

/// Per-profile override layer. `None` for any field means "fall through
/// to the [`ProviderSpec`] default". Stored in the `profile` DB row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub small_fast_model: Option<String>,
    pub default_opus_model: Option<String>,
    pub default_sonnet_model: Option<String>,
    pub default_haiku_model: Option<String>,
    pub subagent_model: Option<String>,
}

/// Resolved spawn-time env: `(name, value)` pairs to set on the child.
/// Built from a [`ProviderSpec`] + [`Overrides`] + auth_token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEnv {
    pub vars: Vec<(String, String)>,
}

/// Static provider registry. Order = display order in CLI pickers.
/// Tier should be non-decreasing as you go down the list (T1, T2, T3).
pub const PROVIDERS: &[ProviderSpec] = &[
    // ── Tier 1 ──────────────────────────────────────────────────────
    ProviderSpec {
        id: "anthropic",
        display_name: "Anthropic",
        backend: "claude",
        tier: Tier::T1,
        supported_auth: &["login", "api_key"],
        base_url: None,
        model: None,
        small_fast_model: None,
        default_opus_model: None,
        default_sonnet_model: None,
        default_haiku_model: None,
        subagent_model: None,
        extra_env: &[],
    },
    ProviderSpec {
        id: "openai",
        display_name: "OpenAI",
        backend: "codex",
        tier: Tier::T1,
        supported_auth: &["login", "api_key"],
        base_url: None,
        model: None,
        small_fast_model: None,
        default_opus_model: None,
        default_sonnet_model: None,
        default_haiku_model: None,
        subagent_model: None,
        extra_env: &[],
    },
    // ── Tier 2 ──────────────────────────────────────────────────────
    ProviderSpec {
        id: "deepseek",
        display_name: "DeepSeek",
        backend: "claude",
        tier: Tier::T2,
        supported_auth: &["api_key"],
        base_url: Some("https://api.deepseek.com/anthropic"),
        model: Some("deepseek-v4-pro"),
        small_fast_model: None,
        default_opus_model: Some("deepseek-v4-pro"),
        default_sonnet_model: Some("deepseek-v4-pro"),
        default_haiku_model: Some("deepseek-v4-flash"),
        subagent_model: Some("deepseek-v4-flash"),
        extra_env: &[("CLAUDE_CODE_EFFORT_LEVEL", "max")],
    },
    // ── Tier 3 ──────────────────────────────────────────────────────
    ProviderSpec {
        id: "kimi",
        display_name: "Kimi (Moonshot)",
        backend: "claude",
        tier: Tier::T3,
        supported_auth: &["api_key"],
        base_url: Some("https://api.moonshot.ai/anthropic"),
        model: Some("kimi-k2.5"),
        small_fast_model: None,
        default_opus_model: Some("kimi-k2.5"),
        default_sonnet_model: Some("kimi-k2.5"),
        default_haiku_model: Some("kimi-k2.5"),
        subagent_model: Some("kimi-k2.5"),
        extra_env: &[("ENABLE_TOOL_SEARCH", "false")],
    },
    ProviderSpec {
        id: "minimax",
        display_name: "MiniMax",
        backend: "claude",
        tier: Tier::T3,
        supported_auth: &["api_key"],
        base_url: Some("https://api.minimax.io/anthropic"),
        model: Some("MiniMax-M2.7"),
        small_fast_model: None,
        default_opus_model: Some("MiniMax-M2.7"),
        default_sonnet_model: Some("MiniMax-M2.7"),
        default_haiku_model: Some("MiniMax-M2.7"),
        subagent_model: None,
        extra_env: &[
            ("API_TIMEOUT_MS", "3000000"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ],
    },
    ProviderSpec {
        id: "zai",
        display_name: "Z.AI (智谱)",
        backend: "claude",
        tier: Tier::T3,
        supported_auth: &["api_key"],
        base_url: Some("https://api.z.ai/api/anthropic"),
        model: None, // server-side mapping
        small_fast_model: None,
        default_opus_model: None,
        default_sonnet_model: None,
        default_haiku_model: None,
        subagent_model: None,
        extra_env: &[("API_TIMEOUT_MS", "3000000")],
    },
    ProviderSpec {
        id: "custom",
        display_name: "Custom (user-supplied base URL)",
        backend: "claude",
        tier: Tier::T3,
        supported_auth: &["api_key"],
        base_url: None, // MUST be overridden on profile
        model: None,
        small_fast_model: None,
        default_opus_model: None,
        default_sonnet_model: None,
        default_haiku_model: None,
        subagent_model: None,
        extra_env: &[],
    },
];

impl ProviderEnv {
    /// Resolve a final env list from `(spec, overrides, auth_token)`.
    /// Dispatches by `spec.backend` so codex profiles get the
    /// OpenAI-namespaced env vars and claude profiles get the
    /// Anthropic-namespaced ones.
    pub fn build(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
        match spec.backend {
            "claude" => Self::build_claude(spec, over, auth_token),
            "codex" => Self::build_codex(spec, over, auth_token),
            other => unreachable!(
                "provider registry guarantees backend ∈ {{claude, codex}}; got {other}"
            ),
        }
    }

    /// Anthropic-namespaced env: ANTHROPIC_BASE_URL / ANTHROPIC_MODEL /
    /// ANTHROPIC_AUTH_TOKEN / CLAUDE_CODE_SUBAGENT_MODEL etc.
    fn build_claude(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
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

    /// OpenAI/codex-namespaced env: OPENAI_BASE_URL / OPENAI_API_KEY /
    /// CODEX_MODEL.
    ///
    /// Codex auth is normally `<CODEX_HOME>/auth.json`; ProviderEnv is
    /// used only when a profile carries an explicit API key (api_key
    /// auth method). OAuth-authenticated codex profiles bypass
    /// ProviderEnv entirely just like claude OAuth.
    ///
    /// claude-only fields on Overrides (opus/sonnet/haiku/subagent tier
    /// names) are ignored — codex does not have an equivalent tier
    /// concept at the env-var surface.
    fn build_codex(spec: &ProviderSpec, over: &Overrides, auth_token: String) -> Self {
        let mut vars: Vec<(String, String)> = Vec::new();

        let pick = |o: &Option<String>, s: Option<&'static str>| -> Option<String> {
            o.clone().or_else(|| s.map(String::from))
        };
        if let Some(v) = pick(&over.base_url, spec.base_url) {
            vars.push(("OPENAI_BASE_URL".into(), v));
        }
        vars.push(("OPENAI_API_KEY".into(), auth_token));
        if let Some(v) = pick(&over.model, spec.model) {
            vars.push(("CODEX_MODEL".into(), v));
        }
        for (k, v) in spec.extra_env {
            vars.push(((*k).to_string(), (*v).to_string()));
        }
        Self { vars }
    }
}

/// Look up a provider by id. Returns `None` if no entry matches.
pub fn lookup(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Iterate providers for a given backend, in display order (T1 first).
/// Relies on `PROVIDERS` being declared in non-decreasing tier order.
pub fn iter_for_backend<'a>(backend: &'a str) -> impl Iterator<Item = &'static ProviderSpec> + 'a {
    PROVIDERS.iter().filter(move |p| p.backend == backend)
}

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
        assert!(
            d.extra_env
                .iter()
                .any(|(k, v)| *k == "CLAUDE_CODE_EFFORT_LEVEL" && *v == "max")
        );
    }

    #[test]
    fn iter_for_backend_sorts_t1_first() {
        let claude_specs: Vec<&ProviderSpec> = iter_for_backend("claude").collect();
        assert!(!claude_specs.is_empty());
        assert_eq!(
            claude_specs[0].id, "anthropic",
            "T1 anthropic must be first"
        );
    }

    #[test]
    fn iter_for_backend_filters_by_backend() {
        for s in iter_for_backend("claude") {
            assert_eq!(s.backend, "claude");
        }
    }

    fn vars_to_map(env: &ProviderEnv) -> std::collections::HashMap<&str, &str> {
        env.vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn build_uses_spec_defaults_when_no_overrides() {
        let spec = lookup("deepseek").unwrap();
        let env = ProviderEnv::build(spec, &Overrides::default(), "sk-test".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(
            m.get("ANTHROPIC_BASE_URL"),
            Some(&"https://api.deepseek.com/anthropic")
        );
        assert_eq!(m.get("ANTHROPIC_AUTH_TOKEN"), Some(&"sk-test"));
        assert_eq!(m.get("ANTHROPIC_MODEL"), Some(&"deepseek-v4-pro"));
        assert_eq!(
            m.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some(&"deepseek-v4-flash")
        );
        assert_eq!(
            m.get("CLAUDE_CODE_SUBAGENT_MODEL"),
            Some(&"deepseek-v4-flash")
        );
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
        assert_eq!(
            m.get("ANTHROPIC_BASE_URL"),
            Some(&"https://my.proxy/anthropic")
        );
        assert_eq!(m.get("ANTHROPIC_MODEL"), Some(&"custom-model"));
        // Non-overridden field still uses spec default.
        assert_eq!(
            m.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some(&"deepseek-v4-flash")
        );
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
        assert_eq!(
            m.get("ANTHROPIC_BASE_URL"),
            Some(&"https://example.com/api")
        );
    }

    #[test]
    fn build_codex_emits_openai_namespace() {
        let spec = lookup("openai").unwrap();
        let env = ProviderEnv::build(spec, &Overrides::default(), "sk-openai-test".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("OPENAI_API_KEY"), Some(&"sk-openai-test"));
        // openai spec has all None → only API_KEY appears.
        assert!(!m.contains_key("OPENAI_BASE_URL"));
        assert!(!m.contains_key("CODEX_MODEL"));
        // Claude env vars must NOT be set on a codex profile.
        assert!(!m.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!m.contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn build_codex_with_overrides_sets_base_url_and_model() {
        let spec = lookup("openai").unwrap();
        let over = Overrides {
            base_url: Some("https://proxy.example/v1".to_owned()),
            model: Some("gpt-5.5".to_owned()),
            ..Default::default()
        };
        let env = ProviderEnv::build(spec, &over, "sk-x".to_owned());
        let m = vars_to_map(&env);
        assert_eq!(m.get("OPENAI_BASE_URL"), Some(&"https://proxy.example/v1"));
        assert_eq!(m.get("CODEX_MODEL"), Some(&"gpt-5.5"));
        assert_eq!(m.get("OPENAI_API_KEY"), Some(&"sk-x"));
    }
}
