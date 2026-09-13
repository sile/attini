//! Pure permission judgment for `command` tool invocations.
//!
//! Rules are argv-prefix matchers (a rule matches when its
//! `argv_prefix` equals the first N elements of the tool call's
//! `argv`). Evaluation returns [`Judgment::AutoApprove`],
//! [`Judgment::AutoDeny`], or [`Judgment::Pending`]. No I/O — file
//! loading and session record writing live in the impl-layer
//! `crate::permissions` and `crate::tell_cli`.
//!
//! Evaluation order: deny rules (session then workspace) always win,
//! then the first matching approve rule, then the pending fallback
//! for an unmatched command.

/// How side-effecting tool calls are authorized in this invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Authorization {
    /// Normal per-tool-call flow: patches and unmatched commands
    /// require approval via `pending.json`.
    #[default]
    PerTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub argv_prefix: Vec<String>,
    /// `None` for attribute-only rules that carry no decision; such
    /// rules never match (they are kept only to preserve unknown rows
    /// on round-trip through `grant`).
    pub decision: Option<RuleDecision>,
}

impl Rule {
    pub fn new_grant_approve(argv_prefix: Vec<String>) -> Self {
        Self {
            argv_prefix,
            decision: Some(RuleDecision::Approve),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    Session,
    Workspace,
}

impl RuleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgment {
    /// Auto-run the command and record a `tool_approval` with
    /// `decision: "approve"` + sidecar.
    AutoApprove(AutoDecision),
    /// Auto-reject the command and record a `tool_approval` with
    /// `decision: "reject"` + sidecar. The loop continues.
    AutoDeny(AutoDecision),
    /// Fall back to normal pending flow.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDecision {
    pub scope: RuleScope,
    pub argv_prefix: Vec<String>,
    pub reason: AutoReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReason {
    RuleApprove,
    RuleDeny,
}

impl AutoReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuleApprove => "rule_approve",
            Self::RuleDeny => "rule_deny",
        }
    }
}

/// Evaluate a command against the permission rules. Deny rules across
/// both scopes are checked first (session, then workspace); the first
/// matching deny wins over everything. Otherwise the first matching
/// approve rule wins, then the fallback.
pub fn evaluate(
    session_rules: &[Rule],
    workspace_rules: &[Rule],
    argv: &[String],
    _authorization: &Authorization,
) -> Judgment {
    // Deny always wins across every scope.
    for (scope, rules) in [
        (RuleScope::Session, session_rules),
        (RuleScope::Workspace, workspace_rules),
    ] {
        for rule in rules {
            if rule.decision == Some(RuleDecision::Deny) && rule_matches(rule, argv) {
                return Judgment::AutoDeny(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleDeny,
                });
            }
        }
    }
    // First matching approve rule.
    for (scope, rules) in [
        (RuleScope::Session, session_rules),
        (RuleScope::Workspace, workspace_rules),
    ] {
        for rule in rules {
            if !rule_matches(rule, argv) {
                continue;
            }
            match judge_matched(rule, scope) {
                JudgeOutcome::Decided(judgment) => return judgment,
                JudgeOutcome::KeepLooking => continue,
            }
        }
    }
    // No rule matched.
    Judgment::Pending
}

enum JudgeOutcome {
    Decided(Judgment),
    KeepLooking,
}

fn rule_matches(rule: &Rule, argv: &[String]) -> bool {
    if rule.argv_prefix.is_empty() {
        return false;
    }
    if rule.argv_prefix.len() > argv.len() {
        return false;
    }
    rule.argv_prefix
        .iter()
        .zip(argv.iter())
        .all(|(a, b)| a == b)
}

/// Judge a single matching rule. A matched approve rule decides; an
/// attribute-only rule (no decision) keeps looking. A `Pending`-ish
/// outcome is never returned here.
fn judge_matched(rule: &Rule, scope: RuleScope) -> JudgeOutcome {
    match rule.decision {
        Some(RuleDecision::Approve) => JudgeOutcome::Decided(Judgment::AutoApprove(AutoDecision {
            scope,
            argv_prefix: rule.argv_prefix.clone(),
            reason: AutoReason::RuleApprove,
        })),
        None => JudgeOutcome::KeepLooking,
        Some(RuleDecision::Deny) => unreachable!("deny handled before the rule pass"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn rule(decision: Option<RuleDecision>, prefix: &[&str]) -> Rule {
        Rule {
            argv_prefix: argv(prefix),
            decision,
        }
    }

    #[test]
    fn evaluate_default_approve_rule_matches() {
        let r = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        match evaluate(
            &[r],
            &[],
            &argv(&["cargo", "test", "--workspace"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.argv_prefix, argv(&["cargo", "test"])),
            other => panic!("expected AutoApprove, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_attribute_only_rule_pends() {
        let r = rule(None, &["ls"]);
        assert!(matches!(
            evaluate(&[r], &[], &argv(&["ls", "-la"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_deny_wins_across_scopes() {
        let r = rule(Some(RuleDecision::Deny), &["rm", "-rf"]);
        assert!(matches!(
            evaluate(
                std::slice::from_ref(&r),
                &[],
                &argv(&["rm", "-rf", "tmp"]),
                &Authorization::PerTool
            ),
            Judgment::AutoDeny(_)
        ));
    }

    #[test]
    fn evaluate_workspace_deny_beats_session_approve() {
        let ws = rule(Some(RuleDecision::Deny), &["cargo", "test"]);
        let sess = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        // Deny is checked across both scopes before any approve rule.
        match evaluate(
            &[sess],
            &[ws],
            &argv(&["cargo", "test"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoDeny(d) => assert_eq!(d.scope, RuleScope::Workspace),
            other => panic!("expected AutoDeny(workspace), got {other:?}"),
        }
    }

    #[test]
    fn evaluate_rule_prefix_longer_than_argv_does_not_match() {
        let r = rule(Some(RuleDecision::Approve), &["cargo", "test", "--all"]);
        assert!(matches!(
            evaluate(
                &[r],
                &[],
                &argv(&["cargo", "test"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_element_wise_mismatch_does_not_match() {
        let r = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        assert!(matches!(
            evaluate(
                &[r],
                &[],
                &argv(&["cargo", "check"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_empty_argv_prefix_never_matches() {
        let r = rule(Some(RuleDecision::Approve), &[]);
        assert!(matches!(
            evaluate(&[r], &[], &argv(&["ls"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_without_matching_rule_pends() {
        let cargo_test_rule = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        assert!(matches!(
            evaluate(
                &[cargo_test_rule],
                &[],
                &argv(&["bash", "-c", "ls | head"]),
                &Authorization::PerTool,
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_with_matching_rule_auto_approves() {
        let bash_c_rule = rule(Some(RuleDecision::Approve), &["bash", "-c"]);
        match evaluate(
            &[bash_c_rule],
            &[],
            &argv(&["bash", "-c", "ls | head"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.argv_prefix, argv(&["bash", "-c"])),
            other => panic!("expected AutoApprove for approved bash -c, got {other:?}"),
        }
    }
}
