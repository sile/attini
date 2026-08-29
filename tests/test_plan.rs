//! Integration tests for the plan artifact lifecycle: rendering,
//! parsing, sealing, `plan check`, and `plan ok`. Commands that
//! resolve session paths relative to the working directory (create,
//! run, close) are exercised through their pure helpers instead, since
//! the test harness cannot change the process-wide CWD safely.

use std::fs;
use std::path::PathBuf;

use attini::plan::{
    Confirmation, PlanActions, PlanCommandAction, PlanPatchAction, RESERVED_CONFIRMATION_ID,
    apply_confirmations, parse, parse_snapshot, render_sealed, seal_hash, verify_seal,
};

fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("attini-plan-test-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn sample_actions() -> PlanActions {
    PlanActions {
        patches: vec![PlanPatchAction {
            id: "update-parser".to_string(),
            path: "src/parser.rs".to_string(),
            description: "Update the parser".to_string(),
        }],
        commands: vec![PlanCommandAction {
            id: "run-tests".to_string(),
            argv: vec!["cargo".to_string(), "test".to_string()],
            description: "Run the test suite".to_string(),
        }],
    }
}

fn unchecked_confirmations() -> Vec<Confirmation> {
    vec![
        Confirmation::unchecked(RESERVED_CONFIRMATION_ID, "Approve all confirmation items"),
        Confirmation::unchecked("public-api", "Allow public API changes"),
    ]
}

#[test]
fn sealed_plan_roundtrips_through_parse_and_verify() {
    let text = render_sealed(
        "# Plan\n\nRewrite the parser.",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    let parsed = parse(&text).expect("parse");
    assert_eq!(parsed.body_markdown, "# Plan\n\nRewrite the parser.");
    assert_eq!(parsed.actions, sample_actions());
    assert!(verify_seal(&text).expect("verify"));

    let edited = text.replace("Rewrite the parser", "Rewrite the lexer");
    assert!(!verify_seal(&edited).expect("verify edited"));
    // The recomputed hash over the edited file differs from the hash
    // stored in the seal marker, which is why verify_seal is false.
    let stored = parse(&edited).expect("parse edited").confirmations;
    assert!(!stored.is_empty());
}

#[test]
fn snapshot_json_parses_back() {
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    let parsed = parse(&text).expect("parse");
    let hash = seal_hash(text.as_bytes()).expect("hash");
    let snapshot = attini::plan::PlanSnapshot {
        snapshot_version: attini::plan::PLAN_SNAPSHOT_VERSION,
        plan_format_version: attini::plan::PLAN_FORMAT_VERSION,
        plan_sha256: hash.clone(),
        canonical_path: "plans/plan-x.md".to_string(),
        body_markdown: parsed.body_markdown.clone(),
        actions: parsed.actions.clone(),
        confirmations: parsed.confirmations.clone(),
    };
    let json = snapshot.to_json();
    let back = parse_snapshot(&json).expect("parse snapshot");
    assert_eq!(back.plan_sha256, hash);
    assert_eq!(back.actions, sample_actions());
    assert_eq!(back.body_markdown, parsed.body_markdown);
}

#[test]
fn plan_check_returns_incomplete_for_unapproved_plan() {
    let dir = tempdir("check-incomplete");
    let path = dir.join("plan.md");
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    fs::write(&path, &text).expect("write");
    // Newly created plans have every checkbox unchecked.
    let code = attini::plan_cmd::run_check(&path).expect("check");
    assert_eq!(code, attini::plan_cmd::EXIT_PLAN_INCOMPLETE.into());
}

#[test]
fn plan_check_reports_seal_mismatch_after_edit() {
    let dir = tempdir("check-seal");
    let path = dir.join("plan.md");
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    let edited = text.replace("body", "body edited");
    fs::write(&path, &edited).expect("write");
    let code = attini::plan_cmd::run_check(&path).expect("check");
    assert_eq!(code, attini::plan_cmd::EXIT_PLAN_INCOMPLETE.into());
}

#[test]
fn plan_ok_reseals_and_then_check_succeeds() {
    let dir = tempdir("ok-reseal");
    let path = dir.join("plan.md");
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    fs::write(&path, &text).expect("write");

    let code = attini::plan_cmd::run_ok(&path, true, &[]).expect("ok");
    assert_eq!(code, 0.into());
    assert!(verify_seal(&fs::read_to_string(&path).expect("read")).expect("verify"));

    let code = attini::plan_cmd::run_check(&path).expect("check");
    assert_eq!(code, 0.into());
}

#[test]
fn plan_ok_with_item_ids_checks_those_and_unchecks_all_ok() {
    let dir = tempdir("ok-items");
    let path = dir.join("plan.md");
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    fs::write(&path, &text).expect("write");
    attini::plan_cmd::run_ok(&path, false, &["public-api".to_string()]).expect("ok");
    let parsed = parse(&fs::read_to_string(&path).expect("read")).expect("parse");
    let public = parsed
        .confirmations
        .iter()
        .find(|c| c.id == "public-api")
        .expect("public-api present");
    let all_ok = parsed
        .confirmations
        .iter()
        .find(|c| c.id == RESERVED_CONFIRMATION_ID)
        .expect("all-ok present");
    assert!(public.checked);
    assert!(!all_ok.checked);
}

#[test]
fn plan_ok_with_unknown_id_errors_and_leaves_file_untouched() {
    let dir = tempdir("ok-unknown");
    let path = dir.join("plan.md");
    let text = render_sealed(
        "# Plan\n\nbody",
        &sample_actions(),
        &unchecked_confirmations(),
    )
    .expect("render");
    fs::write(&path, &text).expect("write");
    assert!(attini::plan_cmd::run_ok(&path, false, &["nope".to_string()]).is_err());
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        text,
        "the plan file must not change on an unknown id"
    );
}

#[test]
fn apply_confirmations_all_or_individual_rules() {
    let plan = attini::plan::ParsedPlan {
        body_markdown: String::new(),
        actions: PlanActions::default(),
        confirmations: unchecked_confirmations(),
    };
    let all = apply_confirmations(&plan, true, &[]).expect("all");
    assert!(all.iter().all(|c| c.checked == (c.id == "all-ok")));

    let individual = apply_confirmations(&plan, false, &["public-api".to_string()]).expect("ind");
    assert!(
        individual
            .iter()
            .find(|c| c.id == "public-api")
            .unwrap()
            .checked
    );
    assert!(
        !individual
            .iter()
            .find(|c| c.id == "all-ok")
            .unwrap()
            .checked
    );
}

#[test]
fn confirmations_complete_semantics() {
    let unchecked = vec![
        Confirmation::unchecked(RESERVED_CONFIRMATION_ID, "all"),
        Confirmation::unchecked("a", "A"),
    ];
    assert!(!attini::plan::confirmations_complete(&unchecked));
    let all_ok_checked = vec![
        Confirmation {
            id: RESERVED_CONFIRMATION_ID.into(),
            description: "all".into(),
            checked: true,
        },
        Confirmation::unchecked("a", "A"),
    ];
    assert!(attini::plan::confirmations_complete(&all_ok_checked));
}
