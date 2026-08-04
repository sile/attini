//! Integration tests for the JSON Lines session transcript writer.

use std::time::Duration;

use attini::sansio::agent::AgentMetrics;
use attini::tui::transcript::{
    ApprovalDecision, AssistantToolCall, COMMAND_OUTPUT_PREVIEW_MAX_BYTES, CommandStream,
    MetricsCounters, SessionEndReason, TranscriptRecord, TranscriptWriter,
};
use tempdir_alt::TempDir;
use tokio::fs;

mod tempdir_alt {
    //! Minimal temp-dir helper that avoids pulling in a new crate
    //! for a handful of tests. Creates a unique directory under
    //! `std::env::temp_dir()`, and best-effort deletes it on drop.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn new(prefix: &str) -> std::io::Result<Self> {
            let mut path = std::env::temp_dir();
            let pid = std::process::id();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            path.push(format!("attini-transcript-{prefix}-{pid}-{id}"));
            std::fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn kind(line: &str) -> String {
    let json = nojson::RawJson::parse(line).expect("valid JSON line");
    json.value()
        .to_member("kind")
        .expect("kind member")
        .required()
        .expect("kind required")
        .to_unquoted_string_str()
        .expect("kind is a string")
        .into_owned()
}

// ---- individual record kinds ---------------------------------------

#[test]
fn session_start_record_shape() {
    let json = TranscriptRecord::SessionStart {
        ts: 1_700_000_000_000,
        attini_version: "0.0.1".into(),
        model: "deepseek-v4-flash".into(),
        workspace: "/tmp/ws".into(),
    }
    .to_json_string();
    assert!(json.contains(r#""kind":"session_start""#));
    assert!(json.contains(r#""ts":1700000000000"#));
    assert!(json.contains(r#""attini_version":"0.0.1""#));
    assert!(json.contains(r#""model":"deepseek-v4-flash""#));
    assert!(json.contains(r#""workspace":"/tmp/ws""#));
}

#[test]
fn assistant_message_record_shape() {
    let json = TranscriptRecord::AssistantMessage {
        ts: 42,
        content: "hi".into(),
        reasoning: Some("think".into()),
        tool_calls: vec![AssistantToolCall {
            id: "call_1".into(),
            function_name: "read".into(),
            arguments_json: r#"{"path":"a"}"#.into(),
        }],
    }
    .to_json_string();
    assert!(json.contains(r#""kind":"assistant_message""#));
    assert!(json.contains(r#""content":"hi""#));
    assert!(json.contains(r#""reasoning":"think""#));
    assert!(json.contains(r#""function_name":"read""#));
    assert!(json.contains(r#""arguments_json":"{\"path\":\"a\"}""#));
}

#[test]
fn assistant_message_reasoning_null_when_absent() {
    let json = TranscriptRecord::AssistantMessage {
        ts: 1,
        content: "hi".into(),
        reasoning: None,
        tool_calls: Vec::new(),
    }
    .to_json_string();
    assert!(json.contains(r#""reasoning":null"#));
    assert!(json.contains(r#""tool_calls":[]"#));
}

#[test]
fn tool_result_record_does_not_carry_is_error_flag() {
    let json = TranscriptRecord::ToolResult {
        ts: 1,
        call_id: "call_1".into(),
        content: r#"{"error":"outside_workspace"}"#.into(),
    }
    .to_json_string();
    assert!(json.contains(r#""kind":"tool_result""#));
    assert!(json.contains(r#""content":"{\"error\":\"outside_workspace\"}""#));
    // The `is_error` field was explicitly removed in the polished
    // design — readers derive error status from `content` instead.
    assert!(!json.contains("is_error"));
}

#[test]
fn patch_preview_ready_record_shape() {
    let json = TranscriptRecord::PatchPreviewReady {
        ts: 1,
        call_id: "call_1".into(),
        target_paths: vec!["a.txt".into(), "b.txt".into()],
        added_lines: 5,
        removed_lines: 2,
        edit_count: 3,
    }
    .to_json_string();
    assert!(json.contains(r#""kind":"patch_preview_ready""#));
    assert!(json.contains(r#""target_paths":["a.txt","b.txt"]"#));
    assert!(json.contains(r#""added_lines":5"#));
    assert!(json.contains(r#""removed_lines":2"#));
    assert!(json.contains(r#""edit_count":3"#));
}

#[test]
fn tool_approval_record_shape() {
    let approve = TranscriptRecord::ToolApproval {
        ts: 1,
        call_id: "c".into(),
        decision: ApprovalDecision::Approve,
    }
    .to_json_string();
    assert!(approve.contains(r#""decision":"approve""#));
    let reject = TranscriptRecord::ToolApproval {
        ts: 1,
        call_id: "c".into(),
        decision: ApprovalDecision::Reject,
    }
    .to_json_string();
    assert!(reject.contains(r#""decision":"reject""#));
}

#[test]
fn command_output_chunk_record_shape() {
    let json = TranscriptRecord::CommandOutputChunk {
        ts: 1,
        call_id: "c".into(),
        stream: CommandStream::Stderr,
        bytes_len: 42,
        preview: "hello".into(),
    }
    .to_json_string();
    assert!(json.contains(r#""kind":"command_output_chunk""#));
    assert!(json.contains(r#""stream":"stderr""#));
    assert!(json.contains(r#""bytes_len":42"#));
    assert!(json.contains(r#""preview":"hello""#));
}

#[test]
fn cancel_record_has_only_kind_and_ts() {
    let json = TranscriptRecord::Cancel { ts: 7 }.to_json_string();
    // Exact-match the whole line: source constraint check that
    // there is no `reason` field (rejected in polish).
    assert_eq!(json, r#"{"kind":"cancel","ts":7}"#);
}

#[test]
fn finish_record_carries_optional_reason() {
    let with = TranscriptRecord::Finish {
        ts: 1,
        reason: Some("stop".into()),
    }
    .to_json_string();
    assert!(with.contains(r#""reason":"stop""#));
    let without = TranscriptRecord::Finish {
        ts: 1,
        reason: None,
    }
    .to_json_string();
    assert!(without.contains(r#""reason":null"#));
}

#[test]
fn session_end_reason_serialises_to_stable_strings() {
    let quit = TranscriptRecord::SessionEnd {
        ts: 1,
        reason: SessionEndReason::UserQuit,
    }
    .to_json_string();
    assert!(quit.contains(r#""reason":"user_quit""#));
    let eof = TranscriptRecord::SessionEnd {
        ts: 1,
        reason: SessionEndReason::Eof,
    }
    .to_json_string();
    assert!(eof.contains(r#""reason":"eof""#));
}

// ---- secrets ---------------------------------------------------------

#[test]
fn session_start_never_carries_authorization_material() {
    let json = TranscriptRecord::SessionStart {
        ts: 1,
        attini_version: "0.0.1".into(),
        model: "deepseek-v4-flash".into(),
        workspace: "/tmp".into(),
    }
    .to_json_string();
    let lowered = json.to_ascii_lowercase();
    assert!(!lowered.contains("authorization"));
    assert!(!lowered.contains("bearer"));
    assert!(!lowered.contains("api_key"));
    assert!(!lowered.contains("api-key"));
    assert!(!json.contains("sk-"));
}

// ---- reader forward-compat -----------------------------------------

#[test]
fn reader_can_skip_unknown_kind_and_continue() {
    // Simulate a reader that stripes over JSON Lines and skips
    // lines whose `kind` is not recognised. This is the contract
    // future kinds (retry, backoff, ...) are added under.
    let known: [&str; 12] = [
        "session_start",
        "user_message",
        "assistant_message",
        "tool_result",
        "patch_preview_ready",
        "tool_approval",
        "command_output_chunk",
        "cancel",
        "transport_error",
        "finish",
        "session_end",
        "metrics_snapshot",
    ];
    let file = concat!(
        r#"{"kind":"user_message","ts":1,"text":"hi"}"#,
        "\n",
        r#"{"kind":"future_retry","ts":2,"whatever":true}"#,
        "\n",
        r#"{"kind":"finish","ts":3,"reason":"stop"}"#,
        "\n",
    );
    let mut kept = Vec::new();
    for line in file.lines() {
        let k = kind(line);
        if known.iter().any(|expected| *expected == k) {
            kept.push(k);
        }
    }
    assert_eq!(kept, vec!["user_message".to_string(), "finish".to_string()]);
}

// ---- metrics snapshot ----------------------------------------------

#[test]
fn metrics_snapshot_record_has_kind_ts_and_counters() {
    let counters = MetricsCounters::from_agent_metrics(&AgentMetrics::default());
    let json = TranscriptRecord::MetricsSnapshot { ts: 42, counters }.to_json_string();
    assert!(json.contains(r#""kind":"metrics_snapshot""#));
    assert!(json.contains(r#""ts":42"#));
    assert!(json.contains(r#""counters":{"#));
}

#[test]
fn metrics_snapshot_counters_cover_every_agent_metrics_field() {
    // Baseline: default AgentMetrics has all counters at 0. The
    // JSON must include every field name from AgentMetrics so
    // downstream jq / parsers see a stable, complete map.
    let counters = MetricsCounters::from_agent_metrics(&AgentMetrics::default());
    let json = TranscriptRecord::MetricsSnapshot { ts: 0, counters }.to_json_string();
    // The 33 counter names from AgentMetrics as of this release.
    // Adding a counter here without adding one to MetricsCounters
    // will fail on the compile-time snapshot builder, so this
    // list is the last-line reader-facing contract check.
    let expected = [
        "user_messages_accepted",
        "user_messages_rejected_while_active",
        "cancels_applied",
        "cancels_ignored_when_idle",
        "content_deltas_appended",
        "content_deltas_dropped_as_stale",
        "reasoning_deltas_appended",
        "reasoning_deltas_dropped_as_stale",
        "finishes_committed",
        "finishes_dropped_as_stale",
        "transport_errors_recorded",
        "transport_errors_dropped_as_stale",
        "timeouts_applied",
        "timeouts_dropped_as_stale",
        "tool_call_deltas_appended",
        "tool_call_deltas_dropped_as_stale",
        "tool_call_arguments_fragments_dropped_over_limit",
        "tool_results_committed",
        "tool_results_dropped_as_stale",
        "tool_calls_executed",
        "tool_calls_rejected_by_turn_limit",
        "tool_calls_rejected_by_arguments_limit",
        "patch_calls_previewed",
        "patch_previews_committed",
        "patch_previews_dropped_as_stale",
        "tool_call_approvals_committed",
        "tool_call_approvals_dropped_as_stale",
        "tool_call_rejections_committed",
        "tool_call_rejections_dropped_as_stale",
        "command_calls_dispatched",
        "command_executions_started",
        "command_output_chunks_appended",
        "command_output_chunks_dropped_as_stale",
    ];
    assert_eq!(expected.len(), 33);
    for name in expected {
        let needle = format!(r#""{name}":0"#);
        assert!(
            json.contains(&needle),
            "MetricsSnapshot JSON missing field {name}: {json}"
        );
    }
}

#[test]
fn metrics_snapshot_reflects_incremented_counters() {
    let metrics = AgentMetrics::default();
    metrics.tool_calls_executed.inc();
    metrics.tool_calls_executed.inc();
    metrics.transport_errors_recorded.inc();
    let counters = MetricsCounters::from_agent_metrics(&metrics);
    assert_eq!(counters.tool_calls_executed, 2);
    assert_eq!(counters.transport_errors_recorded, 1);
    assert_eq!(counters.timeouts_applied, 0);
}

// ---- writer end-to-end ---------------------------------------------

#[tokio::test]
async fn writer_writes_session_start_then_records_then_session_end() {
    let dir = TempDir::new("basic").expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    let (writer, _err_rx) = TranscriptWriter::open(
        &path,
        "0.0.1".into(),
        "deepseek-v4-flash".into(),
        "/tmp/ws".into(),
    )
    .await
    .expect("open");
    writer.send(TranscriptRecord::UserMessage {
        ts: 100,
        text: "hi".into(),
    });
    writer.send(TranscriptRecord::Finish {
        ts: 101,
        reason: Some("stop".into()),
    });
    writer.shutdown(SessionEndReason::UserQuit).await;

    let body = fs::read_to_string(&path).await.expect("read");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 4, "session_start + 2 + session_end");
    assert_eq!(kind(lines[0]), "session_start");
    assert_eq!(kind(lines[1]), "user_message");
    assert_eq!(kind(lines[2]), "finish");
    assert_eq!(kind(lines[3]), "session_end");
}

#[tokio::test]
async fn writer_appends_to_existing_file_with_a_fresh_session_start() {
    let dir = TempDir::new("append").expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    tokio::fs::write(&path, "{\"kind\":\"legacy\",\"ts\":0}\n")
        .await
        .expect("seed");
    let (writer, _err_rx) = TranscriptWriter::open(
        &path,
        "0.0.1".into(),
        "deepseek-v4-flash".into(),
        "/tmp/ws".into(),
    )
    .await
    .expect("open");
    writer.shutdown(SessionEndReason::UserQuit).await;
    let body = fs::read_to_string(&path).await.expect("read");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(kind(lines[0]), "legacy");
    assert_eq!(kind(lines[1]), "session_start");
    assert_eq!(kind(lines[2]), "session_end");
}

#[tokio::test]
async fn open_fails_when_parent_dir_missing() {
    let dir = TempDir::new("open-fail").expect("tempdir");
    let path = dir.path().join("does-not-exist").join("transcript.jsonl");
    let res = TranscriptWriter::open(
        &path,
        "0.0.1".into(),
        "deepseek-v4-flash".into(),
        "/tmp/ws".into(),
    )
    .await;
    assert!(res.is_err(), "opening under a missing dir must fail");
}

#[tokio::test]
async fn send_is_non_blocking_and_returns_immediately() {
    // A blast of records should be enqueued far faster than the
    // wall-clock cost of `sleep(1ms)` per record if the mpsc is
    // really unbounded / non-blocking.
    let dir = TempDir::new("nonblocking").expect("tempdir");
    let path = dir.path().join("t.jsonl");
    let (writer, _err_rx) = TranscriptWriter::open(
        &path,
        "0.0.1".into(),
        "deepseek-v4-flash".into(),
        "/tmp/ws".into(),
    )
    .await
    .expect("open");

    let start = std::time::Instant::now();
    for i in 0..500 {
        writer.send(TranscriptRecord::UserMessage {
            ts: i,
            text: "x".into(),
        });
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "500 sends took {elapsed:?}, should have been <50ms"
    );
    writer.shutdown(SessionEndReason::UserQuit).await;
}

// ---- misc -----------------------------------------------------------

#[test]
fn command_preview_helper_bounds_output_length() {
    let bytes = vec![b'a'; COMMAND_OUTPUT_PREVIEW_MAX_BYTES + 10];
    let preview = TranscriptRecord::command_preview_from_bytes(&bytes);
    assert_eq!(preview.len(), COMMAND_OUTPUT_PREVIEW_MAX_BYTES);
}
