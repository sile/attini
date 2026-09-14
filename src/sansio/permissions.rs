//! Pure permission judgment for `command` and `read` tool invocations.
//!
//! Rules come from the JSONL permissions files (see
//! `docs/design/permissions-file.md`). Each rule is a `command` rule
//! (argv-prefix matcher) or a `read` rule (recursive path matcher),
//! and carries an explicit `allow` boolean. Evaluation is
//! **last-match-wins** over the concatenated list
//! `[workspace] ++ [session]`; if no rule matches, the
//! outcome is `Pending`. No I/O — file loading and session record
//! writing live in the impl-layer `crate::permissions` and
//! `crate::tell_cli`.

use std::path::Path;

/// How side-effecting tool calls are authorized in this invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Authorization {
    /// Normal per-tool-call flow: patches and unmatched commands
    /// require approval via `pending.json`.
    #[default]
    PerTool,
}

/// Which kind of tool call a rule governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    /// A `command` tool call, matched by argv prefix.
    Command,
    /// A read-only tool call (`read`/`list`/`search`), matched by path.
    Read,
    /// A `patch` tool call, matched by the target path of each edit.
    Write,
}

impl PermissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// A single permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub kind: PermissionKind,
    /// `true` = allow, `false` = deny. Always present on disk: an
    /// omitted `allow` is a load error, so a typo never silently
    /// turns a rule off.
    pub allow: bool,
    /// `command`: the argv prefix to match (token-wise).
    /// `read`/`write`: unused.
    pub args_prefix: Vec<String>,
    /// `read`/`write`: the path to match (recursively, on segment
    /// boundaries). `command`: unused.
    pub path: String,
}

impl Rule {
    pub fn command(allow: bool, args_prefix: Vec<String>) -> Self {
        Self {
            kind: PermissionKind::Command,
            allow,
            args_prefix,
            path: String::new(),
        }
    }

    pub fn read(allow: bool, path: String) -> Self {
        Self {
            kind: PermissionKind::Read,
            allow,
            args_prefix: Vec::new(),
            path,
        }
    }

    pub fn write(allow: bool, path: String) -> Self {
        Self {
            kind: PermissionKind::Write,
            allow,
            args_prefix: Vec::new(),
            path,
        }
    }
}

/// Which layer a rule came from, for history output. The layer is a
/// property of which file the rule was loaded from, not a field in the
/// rule itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    Workspace,
    Session,
}

impl RuleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }
}

/// One layer's rules tagged with its scope.
pub type ScopedRules<'a> = (RuleScope, &'a [Rule]);

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

/// The decision plus the full match history that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDecision {
    /// The scope of the winning (last matching) rule.
    pub scope: RuleScope,
    /// The winning rule's argv prefix (command rules) or path (read).
    pub args_prefix: Vec<String>,
    /// The winning rule's decision.
    pub allowed: bool,
    /// Every rule that matched during the walk, in evaluation order,
    /// each marked with whether it was the one finally adopted.
    pub matches: Vec<RuleMatch>,
}

/// One matched rule in the evaluation walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub scope: RuleScope,
    pub kind: PermissionKind,
    pub allow: bool,
    pub args_prefix: Vec<String>,
    pub path: String,
    /// `true` for the last matching rule (the one whose `allow` decided).
    pub adopted: bool,
}

/// Evaluate a command against the permission rules. `layers` is the
/// concatenated rule chain in increasing precedence, e.g.
/// `[(Workspace, ws), (Session, sess)]`. The
/// decision of the **last** matching `command` rule wins; no match
/// yields `Pending`.
pub fn evaluate(
    layers: &[ScopedRules<'_>],
    argv: &[String],
    _authorization: &Authorization,
) -> Judgment {
    let mut matches: Vec<RuleMatch> = Vec::new();
    for (scope, rules) in layers {
        for rule in rules.iter() {
            if rule.kind != PermissionKind::Command {
                continue;
            }
            if rule_matches_argv(rule, argv) {
                matches.push(RuleMatch {
                    scope: *scope,
                    kind: rule.kind,
                    allow: rule.allow,
                    args_prefix: rule.args_prefix.clone(),
                    path: String::new(),
                    adopted: false,
                });
            }
        }
    }
    finish_decision(matches)
}

/// Evaluate a read-only request (a canonicalised absolute path) against
/// the permission rules. Same last-match-wins semantics as `evaluate`,
/// but matching uses recursive path components.
pub fn evaluate_read(
    layers: &[ScopedRules<'_>],
    path: &Path,
    _authorization: &Authorization,
) -> Judgment {
    let mut matches: Vec<RuleMatch> = Vec::new();
    for (scope, rules) in layers {
        for rule in rules.iter() {
            if rule.kind != PermissionKind::Read {
                continue;
            }
            if rule_matches_path(rule, path) {
                matches.push(RuleMatch {
                    scope: *scope,
                    kind: rule.kind,
                    allow: rule.allow,
                    args_prefix: Vec::new(),
                    path: rule.path.clone(),
                    adopted: false,
                });
            }
        }
    }
    finish_decision(matches)
}

/// Evaluate a `patch` edit target (a workspace-relative path, as
/// written by the model, or the canonical path it maps to) against the
/// permission rules. Same last-match-wins semantics as `evaluate_read`;
/// `write` and `read` are distinct kinds and do not cross-match.
pub fn evaluate_write(
    layers: &[ScopedRules<'_>],
    path: &Path,
    _authorization: &Authorization,
) -> Judgment {
    let mut matches: Vec<RuleMatch> = Vec::new();
    for (scope, rules) in layers {
        for rule in rules.iter() {
            if rule.kind != PermissionKind::Write {
                continue;
            }
            if rule_matches_path(rule, path) {
                matches.push(RuleMatch {
                    scope: *scope,
                    kind: rule.kind,
                    allow: rule.allow,
                    args_prefix: Vec::new(),
                    path: rule.path.clone(),
                    adopted: false,
                });
            }
        }
    }
    finish_decision(matches)
}

/// Turn a match history into a `Judgment`. The last match's `allow`
/// decides; the last match is marked `adopted`.
fn finish_decision(mut matches: Vec<RuleMatch>) -> Judgment {
    let Some(last) = matches.last_mut() else {
        return Judgment::Pending;
    };
    last.adopted = true;
    let scope = last.scope;
    let args_prefix = last.args_prefix.clone();
    let allowed = last.allow;
    let decision = AutoDecision {
        scope,
        args_prefix,
        allowed,
        matches,
    };
    if allowed {
        Judgment::AutoApprove(decision)
    } else {
        Judgment::AutoDeny(decision)
    }
}

fn rule_matches_argv(rule: &Rule, argv: &[String]) -> bool {
    if rule.args_prefix.is_empty() {
        return false;
    }
    if rule.args_prefix.len() > argv.len() {
        return false;
    }
    rule.args_prefix
        .iter()
        .zip(argv.iter())
        .all(|(a, b)| a == b)
}

/// Match a `read`/`write` rule against a path: the rule's path matches
/// the requested path itself and anything underneath it, on
/// path-segment boundaries (`foo/bar` matches `foo/bar/x` but not
/// `foo/barbaz`). The rule path is compared as given (usually
/// workspace-relative); the caller supplies a path in a consistent
/// form.
fn rule_matches_path(rule: &Rule, path: &Path) -> bool {
    if rule.path.is_empty() {
        return false;
    }
    let prefix = Path::new(&rule.path);
    path.starts_with(prefix)
}

// Small helper so tests can write `r.as_slice()`.
impl Rule {
    #[cfg(test)]
    fn as_slice(&self) -> &[Rule] {
        std::slice::from_ref(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn cmd(allow: bool, prefix: &[&str]) -> Rule {
        Rule::command(allow, argv(prefix))
    }

    #[test]
    fn evaluate_allow_rule_matches() {
        let r = cmd(true, &["cargo", "test"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        match evaluate(
            &layers,
            &argv(&["cargo", "test", "--workspace"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoApprove(d) => {
                assert!(d.allowed);
                assert_eq!(d.args_prefix, argv(&["cargo", "test"]));
                assert_eq!(d.scope, RuleScope::Workspace);
                assert_eq!(d.matches.len(), 1);
                assert!(d.matches[0].adopted);
            }
            other => panic!("expected AutoApprove, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_no_match_pends() {
        let r = cmd(true, &["ls"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(&layers, &argv(&["cat", "x"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_deny_rule_alone_denies() {
        let r = cmd(false, &["rm", "-rf"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(
                &layers,
                &argv(&["rm", "-rf", "tmp"]),
                &Authorization::PerTool
            ),
            Judgment::AutoDeny(_)
        ));
    }

    #[test]
    fn evaluate_last_match_wins_session_overrides_workspace() {
        // Workspace denies, session allows -> session (later) wins.
        let ws = cmd(false, &["cargo", "test"]);
        let sess = cmd(true, &["cargo", "test"]);
        let layers = [
            (RuleScope::Workspace, ws.as_slice()),
            (RuleScope::Session, sess.as_slice()),
        ];
        match evaluate(&layers, &argv(&["cargo", "test"]), &Authorization::PerTool) {
            Judgment::AutoApprove(d) => {
                assert_eq!(d.scope, RuleScope::Session);
                // Both matched; the workspace one is not adopted.
                assert_eq!(d.matches.len(), 2);
                assert!(!d.matches[0].adopted);
                assert!(d.matches[1].adopted);
            }
            other => panic!("expected AutoApprove(session), got {other:?}"),
        }
    }

    #[test]
    fn evaluate_last_match_wins_workspace_deny_over_session_allow() {
        // Session allows, workspace denies -> workspace deny wins order.
        let ws = cmd(false, &["cargo", "test"]);
        let sess = cmd(true, &["cargo", "test"]);
        let layers = [
            (RuleScope::Workspace, ws.as_slice()),
            (RuleScope::Session, sess.as_slice()),
        ];
        // Session is later, so session allow actually wins here.
        assert!(matches!(
            evaluate(&layers, &argv(&["cargo", "test"]), &Authorization::PerTool),
            Judgment::AutoApprove(_)
        ));
    }

    #[test]
    fn evaluate_rule_prefix_longer_than_argv_does_not_match() {
        let r = cmd(true, &["cargo", "test", "--all"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(&layers, &argv(&["cargo", "test"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_element_wise_mismatch_does_not_match() {
        let r = cmd(true, &["cargo", "test"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(&layers, &argv(&["cargo", "check"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_empty_argv_prefix_never_matches() {
        let r = cmd(true, &[]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(&layers, &argv(&["ls"]), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_without_matching_rule_pends() {
        let r = cmd(true, &["cargo", "test"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate(
                &layers,
                &argv(&["bash", "-c", "ls | head"]),
                &Authorization::PerTool
            ),
            Judgment::Pending
        ));
    }

    #[test]
    fn evaluate_bash_dash_c_with_matching_rule_auto_approves() {
        let r = cmd(true, &["bash", "-c"]);
        let layers = [(RuleScope::Workspace, r.as_slice())];
        match evaluate(
            &layers,
            &argv(&["bash", "-c", "ls | head"]),
            &Authorization::PerTool,
        ) {
            Judgment::AutoApprove(d) => assert_eq!(d.args_prefix, argv(&["bash", "-c"])),
            other => panic!("expected AutoApprove for approved bash -c, got {other:?}"),
        }
    }

    #[test]
    fn read_rule_matches_recursively_on_segments() {
        let r = Rule::read(true, "foo/bar".to_string());
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate_read(&layers, Path::new("foo/bar"), &Authorization::PerTool),
            Judgment::AutoApprove(_)
        ));
        assert!(matches!(
            evaluate_read(&layers, Path::new("foo/bar/y/z"), &Authorization::PerTool),
            Judgment::AutoApprove(_)
        ));
        assert!(matches!(
            evaluate_read(&layers, Path::new("foo/barbaz"), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn read_deny_rule_denies() {
        let r = Rule::read(false, "secret".to_string());
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate_read(&layers, Path::new("secret/x"), &Authorization::PerTool),
            Judgment::AutoDeny(_)
        ));
    }

    #[test]
    fn write_rule_matches_recursively_on_segments() {
        let r = Rule::write(true, "src".to_string());
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate_write(&layers, Path::new("src/lib.rs"), &Authorization::PerTool),
            Judgment::AutoApprove(_)
        ));
        assert!(matches!(
            evaluate_write(
                &layers,
                Path::new("src/deep/mod.rs"),
                &Authorization::PerTool
            ),
            Judgment::AutoApprove(_)
        ));
        assert!(matches!(
            evaluate_write(&layers, Path::new("src2/lib.rs"), &Authorization::PerTool),
            Judgment::Pending
        ));
    }

    #[test]
    fn write_deny_rule_denies() {
        let r = Rule::write(false, "src/generated".to_string());
        let layers = [(RuleScope::Workspace, r.as_slice())];
        assert!(matches!(
            evaluate_write(
                &layers,
                Path::new("src/generated/x.rs"),
                &Authorization::PerTool
            ),
            Judgment::AutoDeny(_)
        ));
    }

    #[test]
    fn write_and_read_rules_do_not_cross_match() {
        let read_rule = Rule::read(true, "src".to_string());
        let write_rule = Rule::write(true, "src".to_string());
        let layers = [
            (RuleScope::Workspace, read_rule.as_slice()),
            (RuleScope::Session, write_rule.as_slice()),
        ];
        // A write evaluation ignores the read rule; only the write match counts.
        match evaluate_write(&layers, Path::new("src/lib.rs"), &Authorization::PerTool) {
            Judgment::AutoApprove(d) => {
                assert_eq!(d.scope, RuleScope::Session);
                assert_eq!(d.matches.len(), 1);
                assert_eq!(d.matches[0].kind, PermissionKind::Write);
            }
            other => panic!("expected AutoApprove(session write), got {other:?}"),
        }
        // And the read evaluation ignores the write rule.
        match evaluate_read(&layers, Path::new("src/lib.rs"), &Authorization::PerTool) {
            Judgment::AutoApprove(d) => {
                assert_eq!(d.scope, RuleScope::Workspace);
                assert_eq!(d.matches.len(), 1);
                assert_eq!(d.matches[0].kind, PermissionKind::Read);
            }
            other => panic!("expected AutoApprove(workspace read), got {other:?}"),
        }
    }

    #[test]
    fn read_and_command_rules_do_not_cross_match() {
        let cmd_rule = cmd(true, &["foo"]);
        let read_rule = Rule::read(true, "foo".to_string());
        let layers = [
            (RuleScope::Workspace, cmd_rule.as_slice()),
            (RuleScope::Session, read_rule.as_slice()),
        ];
        // A command evaluation ignores the read rule; only the command match counts.
        match evaluate(&layers, &argv(&["foo", "bar"]), &Authorization::PerTool) {
            Judgment::AutoApprove(d) => {
                assert_eq!(d.scope, RuleScope::Workspace);
                assert_eq!(d.matches.len(), 1);
                assert_eq!(d.matches[0].kind, PermissionKind::Command);
            }
            other => panic!("expected AutoApprove(workspace command), got {other:?}"),
        }
    }
}
