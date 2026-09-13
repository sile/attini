//! Property-based tests for `attini::sansio::permissions::evaluate`
//! focusing on the argv-prefix matching contract.

use attini::sansio::permissions::{Authorization, Judgment, Rule, RuleScope, evaluate};

const ITERATIONS: usize = 512;
const SEED_ENV: &str = "ATTINI_PBT_SEED";

fn sample_token(ctx: &mut noprop::TestCaseContext) -> String {
    let len = noprop::sample_usize_in(ctx, 1..=5);
    noprop::sample_ascii_printable_string(ctx, len)
}

fn sample_argv(ctx: &mut noprop::TestCaseContext, min: usize, max: usize) -> Vec<String> {
    let n = noprop::sample_usize_in(ctx, min..=max);
    (0..n).map(|_| sample_token(ctx)).collect()
}

fn approve_rule(args_prefix: Vec<String>) -> Rule {
    Rule::command(true, args_prefix)
}

#[test]
fn rule_shorter_than_argv_with_matching_head_auto_approves() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        let prefix = sample_argv(ctx, 1, 4);
        let extra_tail = sample_argv(ctx, 0, 4);
        let mut argv = prefix.clone();
        argv.extend(extra_tail);
        let rule = approve_rule(prefix.clone());
        let rules = std::slice::from_ref(&rule);
        let layers = [(RuleScope::Workspace, rules)];
        match evaluate(&layers, &argv, &Authorization::PerTool) {
            Judgment::AutoApprove(d) => {
                assert_eq!(d.args_prefix, prefix);
                Ok(())
            }
            other => {
                panic!("expected AutoApprove for argv {argv:?} vs prefix {prefix:?}, got {other:?}")
            }
        }
    })
}

#[test]
fn rule_longer_than_argv_never_matches() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        // Prefix strictly longer than argv guarantees non-match.
        let argv = sample_argv(ctx, 0, 3);
        let extra = sample_argv(ctx, 1, 3);
        let mut prefix = argv.clone();
        prefix.extend(extra);
        let rule = approve_rule(prefix);
        let rules = std::slice::from_ref(&rule);
        let layers = [(RuleScope::Workspace, rules)];
        assert!(matches!(
            evaluate(&layers, &argv, &Authorization::PerTool),
            Judgment::Pending
        ));
        Ok(())
    })
}

#[test]
fn any_element_mismatch_prevents_match() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        // Build prefix and argv of equal length (>=1), then flip one
        // element in argv to guarantee at least one difference.
        let n = noprop::sample_usize_in(ctx, 1..=4);
        let prefix: Vec<String> = (0..n).map(|_| sample_token(ctx)).collect();
        let flip_at = noprop::sample_usize_in(ctx, 0..=n - 1);
        let mut argv = prefix.clone();
        // Prepend a differentiating character so the flipped token never
        // coincidentally equals the original.
        argv[flip_at] = format!("X_{}", argv[flip_at]);
        let rule = approve_rule(prefix);
        let rules = std::slice::from_ref(&rule);
        let layers = [(RuleScope::Workspace, rules)];
        assert!(matches!(
            evaluate(&layers, &argv, &Authorization::PerTool),
            Judgment::Pending
        ));
        Ok(())
    })
}
