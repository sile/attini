//! Integration tests for the JSON-RPC control interface.
//!
//! Tests cover only paths that do not require a real DeepSeek API
//! (envelope shape, error paths, idle-state queries). Full e2e
//! scenarios that need the model live in
//! `examples/rpc_verify_*.sh` and run manually.

use std::time::Duration;

use attini::deepseek::DeepSeekClient;
use attini::rpc::{RpcConfig, server};
use attini::sansio::agent::AgentMetrics;
use attini::tui::transcript::{
    ApprovalDecision, AssistantToolCall, CommandStream, MetricsCounters, SessionEndReason,
    ToolKind, TranscriptRecord,
};
use nojson::{Json, RawJson};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

// --- notification shape (pure serialisation, no server) ------------

#[test]
fn transcript_records_serialise_to_expected_kinds() {
    let variants: Vec<(&str, TranscriptRecord)> = vec![
        (
            "session_start",
            TranscriptRecord::SessionStart {
                ts: 1,
                attini_version: "0.0.1".into(),
                model: "m".into(),
                workspace: "/w".into(),
            },
        ),
        (
            "user_message",
            TranscriptRecord::UserMessage {
                ts: 2,
                text: "hi".into(),
            },
        ),
        (
            "assistant_message",
            TranscriptRecord::AssistantMessage {
                ts: 3,
                content: "".into(),
                reasoning: None,
                tool_calls: vec![AssistantToolCall {
                    id: "c".into(),
                    function_name: "patch".into(),
                    arguments_json: "{}".into(),
                }],
            },
        ),
        (
            "tool_result",
            TranscriptRecord::ToolResult {
                ts: 4,
                call_id: "c".into(),
                content: "ok".into(),
            },
        ),
        (
            "patch_preview_ready",
            TranscriptRecord::PatchPreviewReady {
                ts: 5,
                call_id: "c".into(),
                target_paths: vec![],
                added_lines: 0,
                removed_lines: 0,
                edit_count: 0,
            },
        ),
        (
            "tool_approval",
            TranscriptRecord::ToolApproval {
                ts: 6,
                call_id: "c".into(),
                decision: ApprovalDecision::Approve,
            },
        ),
        (
            "tool_approval_required",
            TranscriptRecord::ToolApprovalRequired {
                ts: 7,
                call_id: "c".into(),
                tool_kind: ToolKind::Command,
            },
        ),
        (
            "command_output_chunk",
            TranscriptRecord::CommandOutputChunk {
                ts: 8,
                call_id: "c".into(),
                stream: CommandStream::Stdout,
                bytes_len: 0,
                preview: "".into(),
            },
        ),
        ("cancel", TranscriptRecord::Cancel { ts: 9 }),
        (
            "transport_error",
            TranscriptRecord::TransportError {
                ts: 10,
                message: "e".into(),
            },
        ),
        (
            "finish",
            TranscriptRecord::Finish {
                ts: 11,
                reason: None,
            },
        ),
        (
            "session_end",
            TranscriptRecord::SessionEnd {
                ts: 12,
                reason: SessionEndReason::UserQuit,
            },
        ),
        (
            "metrics_snapshot",
            TranscriptRecord::MetricsSnapshot {
                ts: 13,
                counters: MetricsCounters::from_agent_metrics(&AgentMetrics::default()),
            },
        ),
    ];
    assert_eq!(
        variants.len(),
        13,
        "all TranscriptRecord variants must be covered"
    );
    for (expected_kind, record) in variants {
        let json = Json(&record).to_string();
        let parsed = RawJson::parse(&json).expect("parse");
        let kind = parsed
            .value()
            .to_member("kind")
            .expect("kind member")
            .required()
            .expect("kind required")
            .to_unquoted_string_str()
            .expect("kind is string");
        assert_eq!(kind.as_ref(), expected_kind);
    }
}

// --- server integration --------------------------------------------

const TEST_API_KEY: &str = "test-key";

async fn start_server() -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    // Safety: DeepSeekClient::from_env only reads DEEPSEEK_API_KEY at
    // build time; setting it here is idempotent for the test process.
    // The client never actually reaches DeepSeek in these tests
    // because we never trigger a request that would need the network.
    // SAFETY: env access is single-threaded within a #[tokio::test]'s
    // current-thread runtime setup.
    unsafe {
        std::env::set_var("DEEPSEEK_API_KEY", TEST_API_KEY);
    }
    let client = DeepSeekClient::from_env().expect("from_env");
    let config = RpcConfig {
        model: "test-model".into(),
        listen_addr: "127.0.0.1:0".parse().expect("addr"),
        transcript_path: None,
        metrics_snapshot_interval: None,
    };
    // Bind ourselves to know the port before spawning `run`.
    let listener = std::net::TcpListener::bind(config.listen_addr).expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let config = RpcConfig {
        listen_addr: addr,
        ..config
    };
    let handle = tokio::spawn(server::run(client, config));
    // Give the server a moment to bind and print bound_addr.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
}

async fn connect(
    addr: std::net::SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let sock = TcpStream::connect(addr).await.expect("connect");
    let (r, w) = sock.into_split();
    (BufReader::new(r), w)
}

async fn send_request(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    id: u64,
    method: &str,
    params: &str,
) {
    let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#);
    let mut line_with_nl = line;
    line_with_nl.push('\n');
    w.write_all(line_with_nl.as_bytes()).await.expect("write");
    w.flush().await.expect("flush");
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> String {
    let mut s = String::new();
    let fut = r.read_line(&mut s);
    timeout(Duration::from_millis(500), fut)
        .await
        .expect("read timeout")
        .expect("read_line");
    s.trim_end().to_string()
}

async fn read_response_for(id: u64, r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    // Discard notifications until we find a matching response id.
    loop {
        let line = read_line(r).await;
        if line.is_empty() {
            continue;
        }
        let json = RawJson::parse(&line).expect("valid JSON line");
        let id_member = json.value().to_member("id").expect("id member");
        if let Some(id_val) = id_member.optional() {
            let got: u64 = id_val.try_into().expect("id as u64");
            if got == id {
                return line;
            }
        }
        // Notification (no id) — skip.
    }
}

#[tokio::test]
async fn get_state_when_idle_returns_expected_shape() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "get_state", "{}").await;
    let resp = read_response_for(1, &mut r).await;
    let json = RawJson::parse(&resp).expect("parse");
    let result = json
        .value()
        .to_member("result")
        .expect("result member")
        .required()
        .expect("result required");
    let phase = result
        .to_member("phase")
        .expect("phase")
        .required()
        .expect("phase required")
        .to_unquoted_string_str()
        .expect("phase str");
    assert_eq!(phase.as_ref(), "idle");
    let conv_len: u64 = result
        .to_member("conversation_length")
        .expect("conv_len")
        .required()
        .expect("conv_len required")
        .try_into()
        .expect("u64");
    assert_eq!(conv_len, 0);

    // Terminate the server cleanly.
    send_request(&mut w, 2, "quit", "{}").await;
    let _ = read_response_for(2, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn get_metrics_returns_counters_object() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "get_metrics", "{}").await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""counters":{"#));
    assert!(resp.contains(r#""tool_calls_executed":0"#));

    send_request(&mut w, 2, "quit", "{}").await;
    let _ = read_response_for(2, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn get_rendered_grid_returns_shape_with_given_size() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "get_rendered_grid", r#"{"rows":24,"cols":80}"#).await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""size":[24,80]"#));
    assert!(resp.contains(r#""header":{"#));
    assert!(resp.contains(r#""body":{"#));
    assert!(resp.contains(r#""prompt":{"#));

    send_request(&mut w, 2, "quit", "{}").await;
    let _ = read_response_for(2, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn approve_with_no_pending_returns_applied_false() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "approve", r#"{"call_id":"nope"}"#).await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""applied":false"#));

    send_request(&mut w, 2, "reject", r#"{"call_id":"nope"}"#).await;
    let resp2 = read_response_for(2, &mut r).await;
    assert!(resp2.contains(r#""applied":false"#));

    send_request(&mut w, 3, "quit", "{}").await;
    let _ = read_response_for(3, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "nonexistent_method", "{}").await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""code":-32601"#));

    send_request(&mut w, 2, "quit", "{}").await;
    let _ = read_response_for(2, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn malformed_json_returns_parse_error() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    w.write_all(b"not json at all\n").await.expect("write");
    w.flush().await.expect("flush");
    // Parse error response contains `"error":{...}`. The
    // session_start notification (also has no id) does not, so
    // filter on the error member.
    let line = loop {
        let mut s = String::new();
        let fut = r.read_line(&mut s);
        timeout(Duration::from_millis(500), fut)
            .await
            .expect("timeout")
            .expect("read");
        let trimmed = s.trim_end().to_string();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains(r#""error":"#) {
            break trimmed;
        }
    };
    assert!(line.contains(r#""code":-32700"#));

    send_request(&mut w, 99, "quit", "{}").await;
    let _ = read_response_for(99, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn server_emits_session_start_notification_on_connect() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    // First notification should be event/session_start.
    let line = read_line(&mut r).await;
    assert!(line.contains(r#""method":"event/session_start""#));
    assert!(line.contains(r#""kind":"session_start""#));

    send_request(&mut w, 1, "quit", "{}").await;
    let _ = read_response_for(1, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn cancel_when_idle_returns_empty_result() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "cancel", "{}").await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""result":{}"#));

    send_request(&mut w, 2, "quit", "{}").await;
    let _ = read_response_for(2, &mut r).await;
    let _ = timeout(Duration::from_millis(500), handle).await;
}

#[tokio::test]
async fn bind_and_immediately_quit_returns_ok() {
    let (addr, handle) = start_server().await;
    let (mut r, mut w) = connect(addr).await;

    send_request(&mut w, 1, "quit", "{}").await;
    let resp = read_response_for(1, &mut r).await;
    assert!(resp.contains(r#""result":{}"#));
    let joined = timeout(Duration::from_millis(500), handle)
        .await
        .expect("join timeout");
    joined.expect("join ok").expect("run ok");
}
