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
