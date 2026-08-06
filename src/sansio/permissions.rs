//! Pure permission judgment for `command` tool invocations.
//!
//! Rules are argv-prefix matchers (a rule matches when its
//! `argv_prefix` equals the first N elements of the tool call's
//! `argv`). Mode-aware evaluation returns [`Judgment::AutoApprove`],
//! [`Judgment::AutoDeny`], [`Judgment::PlanReject`], or
//! [`Judgment::Pending`]. No I/O — file loading and session record
//! writing live in the impl-layer `crate::permissions` and
//! `crate::agent_cli`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    Plan,
    LocalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub argv_prefix: Vec<String>,
    /// Default `false` (conservative: assume the command may write).
    pub readonly: bool,
    /// Default `true` (conservative: assume the command may use the
    /// network).
    pub network: bool,
    /// `None` for attribute-only rules that only affect mode-based
    /// judgments.
    pub decision: Option<RuleDecision>,
}

impl Rule {
    pub fn new_grant_approve(argv_prefix: Vec<String>) -> Self {
        Self {
            argv_prefix,
            readonly: false,
            network: true,
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
    /// Plan mode couldn't confirm the command is safe. Loop
    /// continues with an error tool_result recorded.
    PlanReject { reason: PlanRejectReason },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRejectReason {
    /// No rule matched and unmatched commands are conservatively
    /// treated as write-capable.
    NoRule,
    /// A rule matched but was not annotated `readonly: true`.
    NotReadonly,
}

impl PlanRejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRule => "no_rule",
            Self::NotReadonly => "not_readonly",
        }
    }
}

/// Rule iteration order for evaluation: `session_rules` first, then
/// `workspace_rules`. First-match-wins within the concatenation.
pub fn evaluate(
    mode: Mode,
    session_rules: &[Rule],
    workspace_rules: &[Rule],
    argv: &[String],
) -> Judgment {
    for (scope, rules) in [
        (RuleScope::Session, session_rules),
        (RuleScope::Workspace, workspace_rules),
    ] {
        for rule in rules {
            if !rule_matches(rule, argv) {
                continue;
            }
            return judge_matched(mode, rule, scope);
        }
    }
    // No rule matched.
    match mode {
        Mode::Default | Mode::LocalOnly => Judgment::Pending,
        Mode::Plan => Judgment::PlanReject {
            reason: PlanRejectReason::NoRule,
        },
    }
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

fn judge_matched(mode: Mode, rule: &Rule, scope: RuleScope) -> Judgment {
    // `deny` always wins across every mode.
    if rule.decision == Some(RuleDecision::Deny) {
        return Judgment::AutoDeny(AutoDecision {
            scope,
            argv_prefix: rule.argv_prefix.clone(),
            reason: AutoReason::RuleDeny,
        });
    }
    match mode {
        Mode::Default => match rule.decision {
            Some(RuleDecision::Approve) => Judgment::AutoApprove(AutoDecision {
                scope,
                argv_prefix: rule.argv_prefix.clone(),
                reason: AutoReason::RuleApprove,
            }),
            None => Judgment::Pending,
            Some(RuleDecision::Deny) => unreachable!("handled above"),
        },
        Mode::Plan => {
            if rule.readonly {
                Judgment::AutoApprove(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleApprove,
                })
            } else {
                Judgment::PlanReject {
                    reason: PlanRejectReason::NotReadonly,
                }
            }
        }
        Mode::LocalOnly => {
            if !rule.network {
                Judgment::AutoApprove(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleApprove,
                })
            } else {
                Judgment::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn evaluate_default_approve_rule_matches() {
        let rule = Rule {
            argv_prefix: argv(&["cargo", "test"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        match evaluate(
            Mode::Default,
            &[rule],
            &[],
            &argv(&["cargo", "test", "--workspace"]),
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.argv_prefix, argv(&["cargo", "test"])),
            other => panic!("expected AutoApprove, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_default_attribute_only_rule_pends() {
        let rule = Rule {
            argv_prefix: argv(&["ls"]),
            readonly: true,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::Default, &[rule], &[], &argv(&["ls", "-la"])),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_deny_wins_across_modes() {
        let rule = Rule {
            argv_prefix: argv(&["rm", "-rf"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Deny),
        };
        for mode in [Mode::Default, Mode::Plan, Mode::LocalOnly] {
            assert!(matches!(
                evaluate(
                    mode,
                    std::slice::from_ref(&rule),
                    &[],
                    &argv(&["rm", "-rf", "tmp"])
                ),
                Judgment::AutoDeny(_)
            ));
        }
    }

    #[test]
    fn evaluate_plan_mode_readonly_matches_auto_approves() {
        let rule = Rule {
            argv_prefix: argv(&["ls"]),
            readonly: true,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::Plan, &[rule], &[], &argv(&["ls", "src"])),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_plan_mode_no_match_rejects() {
        assert!(matches!(
            evaluate(Mode::Plan, &[], &[], &argv(&["rm", "-rf", "tmp"])),
            Judgment::PlanReject {
                reason: PlanRejectReason::NoRule
            }
        ));
    }

    #[test]
    fn evaluate_local_only_network_false_auto_approves() {
        let rule = Rule {
            argv_prefix: argv(&["cargo", "build"]),
            readonly: false,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(
                Mode::LocalOnly,
                &[rule],
                &[],
                &argv(&["cargo", "build", "--release"])
            ),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_local_only_network_true_pends() {
        let rule = Rule {
            argv_prefix: argv(&["curl"]),
            readonly: true,
            network: true,
            decision: None,
        };
        assert!(matches!(
            evaluate(
                Mode::LocalOnly,
                &[rule],
                &[],
                &argv(&["curl", "example.com"])
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_session_wins_over_workspace() {
        let ws = Rule {
            argv_prefix: argv(&["cargo", "test"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Deny),
        };
        let sess = Rule {
            argv_prefix: argv(&["cargo", "test"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        // Session-first: approve wins even though workspace says deny.
        match evaluate(Mode::Default, &[sess], &[ws], &argv(&["cargo", "test"])) {
            Judgment::AutoApprove(d) => assert_eq!(d.scope, RuleScope::Session),
            other => panic!("expected AutoApprove(session), got {other:?}"),
        }
    }

    #[test]
    fn evaluate_rule_prefix_longer_than_argv_does_not_match() {
        let rule = Rule {
            argv_prefix: argv(&["cargo", "test", "--all"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        assert!(matches!(
            evaluate(Mode::Default, &[rule], &[], &argv(&["cargo", "test"])),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_element_wise_mismatch_does_not_match() {
        let rule = Rule {
            argv_prefix: argv(&["cargo", "test"]),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        assert!(matches!(
            evaluate(Mode::Default, &[rule], &[], &argv(&["cargo", "check"])),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_empty_argv_prefix_never_matches() {
        let rule = Rule {
            argv_prefix: Vec::new(),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        assert!(matches!(
            evaluate(Mode::Default, &[rule], &[], &argv(&["ls"])),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_without_matching_rule_pends() {
        // The `["bash", "-c", "..."]` escape hatch does not get an
        // auto-approval unless a rule with `argv_prefix: ["bash", "-c"]`
        // (or a broader ["bash"] prefix) is present. Without it, the
        // call falls through to normal pending approval — proving that
        // pipes / redirects / globs cannot silently ride in on an
        // unrelated approved prefix.
        let cargo_test_rule = approve_rule(&["cargo", "test"]);
        assert!(matches!(
            evaluate(
                Mode::Default,
                &[cargo_test_rule],
                &[],
                &argv(&["bash", "-c", "ls | head"]),
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_with_matching_rule_auto_approves() {
        // Conversely, when the user has explicitly granted
        // `["bash", "-c"]`, calls of that shape auto-approve. This
        // documents the escape-hatch contract: opt-in per rule, not
        // implicit.
        let bash_c_rule = approve_rule(&["bash", "-c"]);
        match evaluate(
            Mode::Default,
            &[bash_c_rule],
            &[],
            &argv(&["bash", "-c", "ls | head"]),
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.argv_prefix, argv(&["bash", "-c"])),
            other => panic!("expected AutoApprove for approved bash -c, got {other:?}"),
        }
    }

    fn approve_rule(prefix: &[&str]) -> Rule {
        Rule {
            argv_prefix: argv(prefix),
            readonly: false,
            network: true,
            decision: Some(RuleDecision::Approve),
        }
    }
}
