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
    /// Cycle order used by the Shift+Tab keybinding. Matches claude's
    /// own TUI cycle when the session is launched with
    /// `--permission-mode bypassPermissions`: bypass → default →
    /// accept-edits → auto and back. Sessions launched in any other
    /// initial mode (e.g., `plan`) see a shorter cycle, so we always
    /// launch in `bypassPermissions` to expose the full rotation.
    ///
    /// `Plan` is intentionally not part of the rotation — it's only
    /// selectable via the `--permission-mode plan` CLI flag at startup.
    pub const CYCLE: [PermissionLevel; 4] = [
        PermissionLevel::BypassAll,
        PermissionLevel::Default,
        PermissionLevel::AcceptEdits,
        PermissionLevel::Auto,
    ];

    /// Every variant, used by code that needs to enumerate all
    /// permission levels (e.g., reverse-mapping codex policy → level).
    pub const ALL: [PermissionLevel; 5] = [
        PermissionLevel::Plan,
        PermissionLevel::Default,
        PermissionLevel::AcceptEdits,
        PermissionLevel::Auto,
        PermissionLevel::BypassAll,
    ];

    /// Next level in the cycle. Wraps around. Levels outside the cycle
    /// (only `Plan`) advance into the cycle at its first slot.
    pub fn next(self) -> Self {
        match Self::CYCLE.iter().position(|&l| l == self) {
            Some(idx) => Self::CYCLE[(idx + 1) % Self::CYCLE.len()],
            None => Self::CYCLE[0],
        }
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

    /// Codex-side policy resolved from a [`PermissionLevel`]. The first
    /// two fields are passed per-turn in the `turn/start` JSON-RPC body;
    /// `reviewer_armed` requires session-level configuration (`-c
    /// approvals_reviewer=auto_review` at codex CLI startup).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cycles_through_all_cycle_levels_in_order() {
        let mut cur = PermissionLevel::BypassAll;
        let order: Vec<_> = (0..4)
            .map(|_| {
                let next = cur.next();
                cur = next;
                next
            })
            .collect();
        assert_eq!(
            order,
            vec![
                PermissionLevel::Default,
                PermissionLevel::AcceptEdits,
                PermissionLevel::Auto,
                PermissionLevel::BypassAll,
            ]
        );
    }

    #[test]
    fn next_from_plan_enters_cycle_at_bypass_all() {
        // Plan is outside the rotation; first Shift+Tab drops you into
        // the cycle at its first slot.
        assert_eq!(PermissionLevel::Plan.next(), PermissionLevel::BypassAll);
    }

    #[test]
    fn label_is_short_and_lowercase() {
        for l in PermissionLevel::ALL {
            let label = l.label();
            assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == ' '));
            assert!(label.len() <= 16);
        }
    }

    #[test]
    fn claude_arg_matches_known_mode_names() {
        assert_eq!(PermissionLevel::Default.claude_arg(), "default");
        assert_eq!(PermissionLevel::AcceptEdits.claude_arg(), "acceptEdits");
        assert_eq!(PermissionLevel::Auto.claude_arg(), "auto");
        assert_eq!(PermissionLevel::BypassAll.claude_arg(), "bypassPermissions");
        assert_eq!(PermissionLevel::Plan.claude_arg(), "plan");
    }

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
}
