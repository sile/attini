//! Integration tests for `attini::subagent` covering the shared
//! read-only parser `attini::session::read_conversation_records` that
//! `subagent_run` relies on. The synchronous self-exec flow itself is
//! covered by the unit tests in `src/subagent.rs` (state
//! classification and pre-spawn decisions), since `subagent::run`
//! resolves session paths relative to the current working directory.

use std::fs;
use std::path::PathBuf;

use attini::session::{
    ApprovalDecision, InvocationEndReason, SessionRecord, read_conversation_records,
};

fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "attini-subagent-test-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

// -----------------------------------------------------------------
// read_conversation_records
// -----------------------------------------------------------------

#[test]
fn read_conversation_records_missing_file_yields_empty_vec() {
    let dir = tempdir("read_missing");
    let path = dir.join("conversation.jsonl");
    let records = read_conversation_records(&path).expect("ok");
    assert!(records.is_empty());
}

#[test]
fn read_conversation_records_parses_all_kinds() {
    let dir = tempdir("read_all_kinds");
    let path = dir.join("conversation.jsonl");
    // A representative line per parseable kind.
    let lines = [
        r#"{"kind":"invocation_start","ts":1,"attini_version":"0.0.1","model":"m"}"#,
        r#"{"kind":"user","ts":2,"text":"hi"}"#,
        r#"{"kind":"assistant","ts":3,"content":"hello","reasoning":null,"tool_calls":[]}"#,
        r#"{"kind":"tool","ts":4,"call_id":"c1","content":"{}"}"#,
        r#"{"kind":"tool_approval","ts":5,"call_id":"c1","decision":"approve"}"#,
        r#"{"kind":"metrics_snapshot","ts":6,"counters":{"turns":2,"tool_errors":0}}"#,
        r#"{"kind":"token_usage","ts":7,"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        r#"{"kind":"summary","ts":8,"since_ts":0,"cutoff_ts":7,"text":"summary body"}"#,
        r#"{"kind":"invocation_end","ts":9,"reason":"completed"}"#,
    ];
    fs::write(&path, lines.join("\n")).expect("write");
    let records = read_conversation_records(&path).expect("ok");
    assert_eq!(records.len(), 9);
    match &records[0] {
        SessionRecord::InvocationStart { model, .. } => assert_eq!(model, "m"),
        other => panic!("expected InvocationStart, got {other:?}"),
    }
    match &records[4] {
        SessionRecord::ToolApproval { decision, .. } => {
            assert_eq!(*decision, ApprovalDecision::Approve);
        }
        other => panic!("expected ToolApproval, got {other:?}"),
    }
    match &records[8] {
        SessionRecord::InvocationEnd { reason, .. } => {
            assert_eq!(*reason, InvocationEndReason::Completed);
        }
        other => panic!("expected InvocationEnd, got {other:?}"),
    }
}

#[test]
fn read_conversation_records_parses_every_invocation_end_reason() {
    let dir = tempdir("read_end_reasons");
    let path = dir.join("conversation.jsonl");
    let lines = [
        r#"{"kind":"invocation_end","ts":1,"reason":"completed"}"#,
        r#"{"kind":"invocation_end","ts":2,"reason":"awaiting_approval"}"#,
        r#"{"kind":"invocation_end","ts":3,"reason":"error"}"#,
        r#"{"kind":"invocation_end","ts":4,"reason":"session_tool_call_exhausted"}"#,
    ];
    fs::write(&path, lines.join("\n")).expect("write");
    let records = read_conversation_records(&path).expect("ok");
    assert_eq!(records.len(), 4);
    let reasons: Vec<InvocationEndReason> = records
        .iter()
        .map(|r| match r {
            SessionRecord::InvocationEnd { reason, .. } => *reason,
            other => panic!("expected InvocationEnd, got {other:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            InvocationEndReason::Completed,
            InvocationEndReason::AwaitingApproval,
            InvocationEndReason::Error,
            InvocationEndReason::SessionToolCallExhausted,
        ]
    );
}

#[test]
fn read_conversation_records_skips_unknown_kinds() {
    let dir = tempdir("read_unknown");
    let path = dir.join("conversation.jsonl");
    let lines = [
        r#"{"kind":"user","ts":1,"text":"hi"}"#,
        r#"{"kind":"future_kind_from_v2","ts":2,"payload":42}"#,
        r#"{"kind":"assistant","ts":3,"content":"hello","reasoning":null,"tool_calls":[]}"#,
    ];
    fs::write(&path, lines.join("\n")).expect("write");
    let records = read_conversation_records(&path).expect("ok");
    // The unknown kind is silently skipped; the surrounding valid
    // records still surface in order.
    assert_eq!(records.len(), 2);
    assert!(matches!(records[0], SessionRecord::User { .. }));
    assert!(matches!(records[1], SessionRecord::Assistant { .. }));
}

#[test]
fn read_conversation_records_tolerates_blank_lines() {
    let dir = tempdir("read_blank");
    let path = dir.join("conversation.jsonl");
    let contents = "\n\n{\"kind\":\"user\",\"ts\":1,\"text\":\"hi\"}\n\n";
    fs::write(&path, contents).expect("write");
    let records = read_conversation_records(&path).expect("ok");
    assert_eq!(records.len(), 1);
}
