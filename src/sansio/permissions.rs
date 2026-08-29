//! Pure permission judgment for `command` tool invocations.
//!
//! Rules are argv-prefix matchers (a rule matches when its
//! `argv_prefix` equals the first N elements of the tool call's
//! `argv`). Mode-aware evaluation returns [`Judgment::AutoApprove`],
//! [`Judgment::AutoDeny`], [`Judgment::PlanReject`],
//! [`Judgment::PlanAction`], [`Judgment::PlanActionNotApproved`], or
//! [`Judgment::Pending`]. No I/O — file loading and session record
//! writing live in the impl-layer `crate::permissions` and
//! `crate::agent_cli`.
//!
//! Evaluation order for a command in [`Authorization::ApprovedPlan`]
//! mode: deny rules (session then workspace) always win, then the
//! first matching approve rule, then an exact plan argv match, and
//! finally a `plan_action_not_approved` error. In
//! [`Authorization::PerTool`] mode an unmatched command falls back to
//! the pending flow.

use crate::plan::ApprovedPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    /// Read-only planning mode used internally by `plan create`:
    /// patch is hidden and only `readonly: true` commands run.
    Planning,
    LocalOnly,
}

/// How side-effecting tool calls are authorized in this invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Authorization {
    /// Normal per-tool-call flow: patches and unmatched commands
    /// require approval via `pending.json`.
    #[default]
    PerTool,
    /// A sealed plan approves an exact set of patch paths and command
    /// argv for this invocation only.
    ApprovedPlan(ApprovedPlan),
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
    /// Planning mode couldn't confirm the command is safe. Loop
    /// continues with an error tool_result recorded.
    PlanReject { reason: PlanRejectReason },
    /// The command exactly matches an approved plan action. Run it
    /// and record the plan action id.
    PlanAction { action_id: String },
    /// The command matches no deny rule, no approve rule, and no plan
    /// action. Reported as an error tool_result (never a pending).
    PlanActionNotApproved,
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

/// Evaluate a command against the permission rules and the
/// invocation authorization. Deny rules across both scopes are
/// checked first (session, then workspace); the first matching deny
/// wins over everything. Otherwise the first matching approve rule
/// wins, then a plan exact-argv match, then the fallback.
pub fn evaluate(
    mode: Mode,
    session_rules: &[Rule],
    workspace_rules: &[Rule],
    argv: &[String],
    authorization: &Authorization,
) -> Judgment {
    // Deny always wins across every mode and scope.
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
            match judge_matched(mode, rule, scope) {
                JudgeOutcome::Decided(judgment) => return judgment,
                JudgeOutcome::KeepLooking => continue,
            }
        }
    }
    // Plan exact-argv match.
    if let Authorization::ApprovedPlan(plan) = authorization {
        if let Some(action) = plan.actions.command_by_argv(argv) {
            return Judgment::PlanAction {
                action_id: action.id.clone(),
            };
        }
        return Judgment::PlanActionNotApproved;
    }
    // No rule matched.
    match mode {
        Mode::Default | Mode::LocalOnly => Judgment::Pending,
        Mode::Planning => Judgment::PlanReject {
            reason: PlanRejectReason::NoRule,
        },
    }
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

/// Judge a single matching rule in `mode`. A matched approve rule
/// decides; an attribute-only rule (no decision) defers to mode
/// handling. A `Pending`-ish outcome is never returned here because
/// plan authorization is applied after the rule pass.
fn judge_matched(mode: Mode, rule: &Rule, scope: RuleScope) -> JudgeOutcome {
    match mode {
        Mode::Default => match rule.decision {
            Some(RuleDecision::Approve) => {
                JudgeOutcome::Decided(Judgment::AutoApprove(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleApprove,
                }))
            }
            None => JudgeOutcome::KeepLooking,
            Some(RuleDecision::Deny) => unreachable!("deny handled before the rule pass"),
        },
        Mode::Planning => {
            if rule.decision == Some(RuleDecision::Approve) || rule.readonly {
                JudgeOutcome::Decided(Judgment::AutoApprove(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleApprove,
                }))
            } else {
                JudgeOutcome::Decided(Judgment::PlanReject {
                    reason: PlanRejectReason::NotReadonly,
                })
            }
        }
        Mode::LocalOnly => {
            if rule.decision == Some(RuleDecision::Approve) || !rule.network {
                JudgeOutcome::Decided(Judgment::AutoApprove(AutoDecision {
                    scope,
                    argv_prefix: rule.argv_prefix.clone(),
                    reason: AutoReason::RuleApprove,
                }))
            } else {
                JudgeOutcome::KeepLooking
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanActions, PlanCommandAction, PlanPatchAction, RESERVED_CONFIRMATION_ID};

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn approved_plan_with_commands(argv_list: Vec<Vec<String>>) -> ApprovedPlan {
        ApprovedPlan {
            plan_sha256: "ab".repeat(32),
            snapshot_path: std::path::PathBuf::from("/tmp/plan-runs/snapshot.json"),
            actions: PlanActions {
                patches: vec![PlanPatchAction {
                    id: "p1".to_string(),
                    path: "src/x.rs".to_string(),
                    description: "patch x".to_string(),
                }],
                commands: argv_list
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| PlanCommandAction {
                        id: format!("c{i}"),
                        argv: a,
                        description: format!("command {i}"),
                    })
                    .collect(),
            },
        }
    }

    fn rule(decision: Option<RuleDecision>, prefix: &[&str]) -> Rule {
        Rule {
            argv_prefix: argv(prefix),
            readonly: false,
            network: true,
            decision,
        }
    }

    #[test]
    fn evaluate_default_approve_rule_matches() {
        let r = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        match evaluate(
            Mode::Default,
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
    fn evaluate_default_attribute_only_rule_pends() {
        let r = rule(None, &["ls"]);
        assert!(matches!(
            evaluate(
                Mode::Default,
                &[r],
                &[],
                &argv(&["ls", "-la"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_deny_wins_across_modes() {
        let r = rule(Some(RuleDecision::Deny), &["rm", "-rf"]);
        for mode in [Mode::Default, Mode::Planning, Mode::LocalOnly] {
            assert!(matches!(
                evaluate(
                    mode,
                    std::slice::from_ref(&r),
                    &[],
                    &argv(&["rm", "-rf", "tmp"]),
                    &Authorization::PerTool
                ),
                Judgment::AutoDeny(_)
            ));
        }
    }

    #[test]
    fn evaluate_planning_mode_readonly_matches_auto_approves() {
        let r = Rule {
            argv_prefix: argv(&["ls"]),
            readonly: true,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(
                Mode::Planning,
                &[r],
                &[],
                &argv(&["ls", "src"]),
                &Authorization::PerTool
            ),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_planning_mode_no_match_rejects() {
        assert!(matches!(
            evaluate(
                Mode::Planning,
                &[],
                &[],
                &argv(&["rm", "-rf", "tmp"]),
                &Authorization::PerTool
            ),
            Judgment::PlanReject {
                reason: PlanRejectReason::NoRule
            }
        ));
    }

    #[test]
    fn evaluate_local_only_network_false_auto_approves() {
        let r = Rule {
            argv_prefix: argv(&["cargo", "build"]),
            readonly: false,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(
                Mode::LocalOnly,
                &[r],
                &[],
                &argv(&["cargo", "build", "--release"]),
                &Authorization::PerTool
            ),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_local_only_network_true_pends() {
        let r = Rule {
            argv_prefix: argv(&["curl"]),
            readonly: true,
            network: true,
            decision: None,
        };
        assert!(matches!(
            evaluate(
                Mode::LocalOnly,
                &[r],
                &[],
                &argv(&["curl", "example.com"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_workspace_deny_beats_session_approve() {
        let ws = rule(Some(RuleDecision::Deny), &["cargo", "test"]);
        let sess = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        // Deny is checked across both scopes before any approve rule.
        match evaluate(
            Mode::Default,
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
                Mode::Default,
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
                Mode::Default,
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
            evaluate(
                Mode::Default,
                &[r],
                &[],
                &argv(&["ls"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_without_matching_rule_pends() {
        let cargo_test_rule = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        assert!(matches!(
            evaluate(
                Mode::Default,
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
            Mode::Default,
            &[bash_c_rule],
            &[],
            &argv(&["bash", "-c", "ls | head"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.argv_prefix, argv(&["bash", "-c"])),
            other => panic!("expected AutoApprove for approved bash -c, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_plan_exact_argv_wins_after_rules() {
        let plan = approved_plan_with_commands(vec![argv(&["cargo", "test"])]);
        match evaluate(
            Mode::Default,
            &[],
            &[],
            &argv(&["cargo", "test"]),
            &Authorization::ApprovedPlan(plan),
        ) {
            Judgment::PlanAction { action_id } => assert_eq!(action_id, "c0"),
            other => panic!("expected PlanAction, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_plan_unmatched_is_not_approved() {
        let plan = approved_plan_with_commands(vec![argv(&["cargo", "test"])]);
        assert!(matches!(
            evaluate(
                Mode::Default,
                &[],
                &[],
                &argv(&["cargo", "publish"]),
                &Authorization::ApprovedPlan(plan)
            ),
            Judgment::PlanActionNotApproved
        ));
    }

    #[test]
    fn evaluate_plan_does_not_override_deny() {
        let deny = rule(Some(RuleDecision::Deny), &["cargo", "test"]);
        let plan = approved_plan_with_commands(vec![argv(&["cargo", "test"])]);
        assert!(matches!(
            evaluate(
                Mode::Default,
                &[deny],
                &[],
                &argv(&["cargo", "test"]),
                &Authorization::ApprovedPlan(plan)
            ),
            Judgment::AutoDeny(_)
        ));
    }

    #[test]
    fn evaluate_plan_rule_approve_wins_before_plan() {
        let approve = rule(Some(RuleDecision::Approve), &["cargo", "test"]);
        let plan = approved_plan_with_commands(vec![argv(&["cargo", "test"])]);
        match evaluate(
            Mode::Default,
            &[approve],
            &[],
            &argv(&["cargo", "test"]),
            &Authorization::ApprovedPlan(plan),
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.reason, AutoReason::RuleApprove),
            other => panic!("expected rule AutoApprove, got {other:?}"),
        }
    }

    #[test]
    fn reserved_all_ok_import_used_in_tests() {
        // Reference the import so the constant stays linked.
        assert_eq!(RESERVED_CONFIRMATION_ID, "all-ok");
    }
}
