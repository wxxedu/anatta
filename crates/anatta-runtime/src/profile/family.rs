//! Family classification for profile-aware conversation segment swap.
//!
//! A family is the **"who validates signatures / encrypted state"** dimension
//! for a backend. Two families coexist per backend in tier 1:
//!
//! - `a-native` / `o-native`: the upstream API strictly validates content
//!   it produced (Anthropic signs `thinking` blocks; OpenAI requires
//!   `encrypted_content` on Reasoning items).
//! - `a-compat` / `o-compat`: 3rd-party endpoints that do not validate
//!   signatures or use the encrypted-reasoning token.
//!
//! The asymmetry that drives sanitization:
//!
//! - **lax → strict**: content from a lax-family source may carry bogus
//!   signatures / fake encrypted blobs; passing it back to a strict API
//!   causes a 400. We must **sanitize** (drop reasoning blocks) on render.
//! - **strict → lax**: strict-family content has valid signatures but
//!   they're harmless to a lax endpoint (it ignores the field). Verbatim
//!   copy is safe.
//!
//! Tier 1 implementation: claude only (`ANative` + `ACompat`). Codex
//! variants (`ONative` + `OCompat`) are defined for forward-compatibility
//! but not exercised.

use serde::{Deserialize, Serialize};

/// Family classification for a profile.
///
/// Two axes:
/// - Backend (a = anthropic / claude family of APIs, o = openai / codex)
/// - Strictness (native = validates upstream, compat = does not)
///
/// `strictness()` orders compat < native; `needs_sanitize` answers
/// "does going from src family to dst family require dropping reasoning
/// blocks?"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Anthropic direct (any Anthropic account). Validates thinking signatures.
    ANative,
    /// 3rd-party Anthropic-compat proxy (DeepSeek, Kimi, MiniMax, …).
    /// Does not validate signatures.
    ACompat,
    /// OpenAI direct (codex). Uses encrypted_content for reasoning continuation.
    ONative,
    /// 3rd-party OpenAI-compat (future). Does not use encrypted_content.
    OCompat,
}

impl Family {
    /// Stricter family → higher value. 0 = lax, 1 = strict.
    pub fn strictness(self) -> u8 {
        match self {
            Family::ACompat | Family::OCompat => 0,
            Family::ANative | Family::ONative => 1,
        }
    }

    /// Round-trip parse from the kebab-case representation used in
    /// `profile.family_override` and `conversation_segments.source_family`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "a-native" => Some(Family::ANative),
            "a-compat" => Some(Family::ACompat),
            "o-native" => Some(Family::ONative),
            "o-compat" => Some(Family::OCompat),
            _ => None,
        }
    }

    /// String form for storage and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::ANative => "a-native",
            Family::ACompat => "a-compat",
            Family::ONative => "o-native",
            Family::OCompat => "o-compat",
        }
    }

    /// True iff (src → dst) crosses the strict boundary going UP. Render
    /// must apply sanitization in that case.
    pub fn needs_sanitize(src: Self, dst: Self) -> bool {
        dst.strictness() > src.strictness()
    }
}

impl Serialize for Family {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Family {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <&str>::deserialize(d)?;
        Family::parse(s).ok_or_else(|| serde::de::Error::custom(format!("invalid family: {s}")))
    }
}

/// Default (backend, provider) → family classification. Used when
/// `profile.family_override` is NULL.
///
/// **Safe-by-default**: unknown providers are classified as `*-compat`
/// (lax). The cost of misclassifying lax as native is a hard API failure;
/// the cost of misclassifying native as lax is only unnecessary stripping
/// of thinking content. Default to lax.
pub fn default_family(backend: BackendKind, provider: &str) -> Family {
    match backend {
        BackendKind::Claude => match provider {
            "anthropic" => Family::ANative,
            _ => Family::ACompat,
        },
        BackendKind::Codex => match provider {
            "openai" => Family::ONative,
            _ => Family::OCompat,
        },
    }
}

/// Resolve a profile's family. Override wins; otherwise use the default
/// derived from `(backend, provider)`.
pub fn family_of(
    backend: BackendKind,
    provider: &str,
    family_override: Option<&str>,
) -> Family {
    if let Some(o) = family_override {
        Family::parse(o).unwrap_or_else(|| default_family(backend, provider))
    } else {
        default_family(backend, provider)
    }
}

/// Backend kind, decoupled from `anatta_store::BackendKind` to keep this
/// crate's profile module independent. The CLI passes the right variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Claude,
    Codex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictness_ordering() {
        assert!(Family::ANative.strictness() > Family::ACompat.strictness());
        assert!(Family::ONative.strictness() > Family::OCompat.strictness());
        assert_eq!(Family::ANative.strictness(), Family::ONative.strictness());
        assert_eq!(Family::ACompat.strictness(), Family::OCompat.strictness());
    }

    #[test]
    fn round_trip_parse() {
        for f in [Family::ANative, Family::ACompat, Family::ONative, Family::OCompat] {
            assert_eq!(Family::parse(f.as_str()), Some(f));
        }
        assert_eq!(Family::parse("nonsense"), None);
    }

    #[test]
    fn needs_sanitize_matrix() {
        // lax → strict requires sanitize
        assert!(Family::needs_sanitize(Family::ACompat, Family::ANative));
        assert!(Family::needs_sanitize(Family::OCompat, Family::ONative));
        // strict → lax does not
        assert!(!Family::needs_sanitize(Family::ANative, Family::ACompat));
        assert!(!Family::needs_sanitize(Family::ONative, Family::OCompat));
        // same family does not
        assert!(!Family::needs_sanitize(Family::ANative, Family::ANative));
        assert!(!Family::needs_sanitize(Family::ACompat, Family::ACompat));
    }

    #[test]
    fn defaults_match_design() {
        assert_eq!(default_family(BackendKind::Claude, "anthropic"), Family::ANative);
        assert_eq!(default_family(BackendKind::Claude, "deepseek"), Family::ACompat);
        assert_eq!(default_family(BackendKind::Claude, "kimi"), Family::ACompat);
        assert_eq!(default_family(BackendKind::Claude, "minimax"), Family::ACompat);
        assert_eq!(default_family(BackendKind::Claude, "custom"), Family::ACompat);
        assert_eq!(default_family(BackendKind::Codex, "openai"), Family::ONative);
        assert_eq!(default_family(BackendKind::Codex, "custom"), Family::OCompat);
    }

    #[test]
    fn override_wins() {
        assert_eq!(
            family_of(BackendKind::Claude, "custom", Some("a-native")),
            Family::ANative,
        );
        assert_eq!(
            family_of(BackendKind::Claude, "anthropic", Some("a-compat")),
            Family::ACompat,
        );
    }

    #[test]
    fn invalid_override_falls_back_to_default() {
        // Defensive: an invalid override string falls back to the
        // (backend, provider) default rather than panicking.
        assert_eq!(
            family_of(BackendKind::Claude, "anthropic", Some("garbage")),
            Family::ANative,
        );
    }

    #[test]
    fn serde_round_trip() {
        let f = Family::ACompat;
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(s, "\"a-compat\"");
        let back: Family = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }
}
