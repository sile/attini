//! Pure permission judgment for `command` tool invocations.
//!
//! Includes a minimal shell-word tokenizer, a safety check for shell
//! metacharacters that make prefix matching unsafe, the rule type
//! itself, and a mode-aware evaluator. No I/O — file loading and
//! session record writing live in the impl-layer `crate::permissions`
//! and `crate::agent_cli`.

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
    pub prefix: String,
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
    pub fn new_grant_approve(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
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
    pub prefix: String,
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
    /// The command_line contained a shell operator (chain / pipe /
    /// redirect / subshell / background). Plan mode cannot verify
    /// safety across shell boundaries.
    ShellOperator,
    /// No rule matched and unmatched commands are conservatively
    /// treated as write-capable.
    NoRule,
    /// A rule matched but was not annotated `readonly: true`.
    NotReadonly,
}

impl PlanRejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShellOperator => "shell_operator",
            Self::NoRule => "no_rule",
            Self::NotReadonly => "not_readonly",
        }
    }
}

/// Tokenize `s` with a minimal shell-word rule:
/// - Split on whitespace runs.
/// - `"..."` and `'...'` are single tokens (quotes stripped).
/// - No escape / variable / brace / glob handling.
///
/// Returns tokens in order. Empty input → empty vector.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
        } else if in_double {
            if c == '"' {
                in_double = false;
            } else {
                current.push(c);
            }
        } else if c == '\'' {
            in_single = true;
        } else if c == '"' {
            in_double = true;
        } else if c.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Return true when the command_line contains any shell operator
/// that makes prefix matching unsafe (chain / pipe / redirect /
/// subshell / background). Detection is done outside quoted spans
/// so `echo 'a && b'` is *not* flagged.
pub fn has_shell_operator(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'|' | b';' | b'<' | b'>' | b'`' => return true,
            b'&' => {
                // `&&` or word-boundary `&` (background); `&>` is bash-only
                // but treat as operator anyway.
                return true;
            }
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Rule iteration order for evaluation: `session_rules` first, then
/// `workspace_rules`. First-match-wins within the concatenation.
pub fn evaluate(
    mode: Mode,
    session_rules: &[Rule],
    workspace_rules: &[Rule],
    command_line: &str,
) -> Judgment {
    if has_shell_operator(command_line) {
        return match mode {
            Mode::Plan => Judgment::PlanReject {
                reason: PlanRejectReason::ShellOperator,
            },
            Mode::Default | Mode::LocalOnly => Judgment::Pending,
        };
    }

    let cmd_tokens = tokenize(command_line);
    for (scope, rules) in [
        (RuleScope::Session, session_rules),
        (RuleScope::Workspace, workspace_rules),
    ] {
        for rule in rules {
            if !rule_matches(rule, &cmd_tokens) {
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

fn rule_matches(rule: &Rule, cmd_tokens: &[String]) -> bool {
    let rule_tokens = tokenize(&rule.prefix);
    if rule_tokens.is_empty() {
        return false;
    }
    if rule_tokens.len() > cmd_tokens.len() {
        return false;
    }
    rule_tokens
        .iter()
        .zip(cmd_tokens.iter())
        .all(|(a, b)| a == b)
}

fn judge_matched(mode: Mode, rule: &Rule, scope: RuleScope) -> Judgment {
    // `deny` always wins across every mode.
    if rule.decision == Some(RuleDecision::Deny) {
        return Judgment::AutoDeny(AutoDecision {
            scope,
            prefix: rule.prefix.clone(),
            reason: AutoReason::RuleDeny,
        });
    }
    match mode {
        Mode::Default => match rule.decision {
            Some(RuleDecision::Approve) => Judgment::AutoApprove(AutoDecision {
                scope,
                prefix: rule.prefix.clone(),
                reason: AutoReason::RuleApprove,
            }),
            None => Judgment::Pending,
            Some(RuleDecision::Deny) => unreachable!("handled above"),
        },
        Mode::Plan => {
            if rule.readonly {
                Judgment::AutoApprove(AutoDecision {
                    scope,
                    prefix: rule.prefix.clone(),
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
                    prefix: rule.prefix.clone(),
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

    #[test]
    fn tokenize_basic() {
        assert_eq!(tokenize("cargo test"), vec!["cargo", "test"]);
        assert_eq!(
            tokenize("cargo  test  --workspace"),
            vec!["cargo", "test", "--workspace"]
        );
        assert_eq!(tokenize("\"cargo\" test"), vec!["cargo", "test"]);
        assert_eq!(tokenize("'cargo' test"), vec!["cargo", "test"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
    }

    #[test]
    fn safety_check_hits_shell_operators() {
        assert!(has_shell_operator("a && b"));
        assert!(has_shell_operator("a || b"));
        assert!(has_shell_operator("a ; b"));
        assert!(has_shell_operator("a | b"));
        assert!(has_shell_operator("a > file"));
        assert!(has_shell_operator("a < file"));
        assert!(has_shell_operator("`whoami`"));
        assert!(has_shell_operator("$(whoami)"));
        assert!(has_shell_operator("sleep 30 &"));
    }

    #[test]
    fn safety_check_ignores_operators_in_quotes() {
        assert!(!has_shell_operator("echo 'a && b'"));
        assert!(!has_shell_operator("echo \"a | b\""));
    }

    #[test]
    fn safety_check_leaves_plain_commands_alone() {
        assert!(!has_shell_operator("cargo test --workspace"));
        assert!(!has_shell_operator("ls -la"));
    }

    #[test]
    fn evaluate_default_approve_rule_matches() {
        let rule = Rule {
            prefix: "cargo test".to_string(),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        match evaluate(Mode::Default, &[rule], &[], "cargo test --workspace") {
            Judgment::AutoApprove(d) => assert_eq!(d.prefix, "cargo test"),
            other => panic!("expected AutoApprove, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_default_attribute_only_rule_pends() {
        let rule = Rule {
            prefix: "ls".to_string(),
            readonly: true,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::Default, &[rule], &[], "ls -la"),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_deny_wins_across_modes() {
        let rule = Rule {
            prefix: "rm -rf".to_string(),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Deny),
        };
        for mode in [Mode::Default, Mode::Plan, Mode::LocalOnly] {
            assert!(matches!(
                evaluate(mode, std::slice::from_ref(&rule), &[], "rm -rf tmp"),
                Judgment::AutoDeny(_)
            ));
        }
    }

    #[test]
    fn evaluate_plan_mode_readonly_matches_auto_approves() {
        let rule = Rule {
            prefix: "ls".to_string(),
            readonly: true,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::Plan, &[rule], &[], "ls src"),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_plan_mode_no_match_rejects() {
        assert!(matches!(
            evaluate(Mode::Plan, &[], &[], "rm -rf tmp"),
            Judgment::PlanReject {
                reason: PlanRejectReason::NoRule
            }
        ));
    }

    #[test]
    fn evaluate_plan_mode_safety_hit_rejects() {
        assert!(matches!(
            evaluate(Mode::Plan, &[], &[], "ls && rm -rf tmp"),
            Judgment::PlanReject {
                reason: PlanRejectReason::ShellOperator
            }
        ));
    }

    #[test]
    fn evaluate_local_only_network_false_auto_approves() {
        let rule = Rule {
            prefix: "cargo build".to_string(),
            readonly: false,
            network: false,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::LocalOnly, &[rule], &[], "cargo build --release"),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_local_only_network_true_pends() {
        let rule = Rule {
            prefix: "curl".to_string(),
            readonly: true,
            network: true,
            decision: None,
        };
        assert!(matches!(
            evaluate(Mode::LocalOnly, &[rule], &[], "curl example.com"),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_session_wins_over_workspace() {
        let ws = Rule {
            prefix: "cargo test".to_string(),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Deny),
        };
        let sess = Rule {
            prefix: "cargo test".to_string(),
            readonly: false,
            network: false,
            decision: Some(RuleDecision::Approve),
        };
        // Session-first: approve wins even though workspace says deny.
        match evaluate(Mode::Default, &[sess], &[ws], "cargo test") {
            Judgment::AutoApprove(d) => assert_eq!(d.scope, RuleScope::Session),
            other => panic!("expected AutoApprove(session), got {other:?}"),
        }
    }
}
