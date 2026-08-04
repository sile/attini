//! Integration tests for `attini::tools::command::run_command`.
//!
//! Spawns real /bin/sh child processes under a scratch workspace,
//! then inspects the returned CommandResult + streamed output.

use std::path::PathBuf;
use std::sync::Arc;

use attini::sansio::agent::{COMMAND_MAX_STREAM_BYTES, CommandInvocation, CommandOutputStream};
use attini::tools::command::{
    CommandOutputMessage, CommandResult, CommandTerminationReason, run_command,
};
use tokio::sync::{Notify, mpsc};

fn cwd() -> PathBuf {
    std::env::temp_dir()
}

fn inv(cmd: &str, timeout: u64) -> CommandInvocation {
    CommandInvocation {
        command_line: cmd.to_string(),
        timeout_seconds: timeout,
    }
}

fn make_channel() -> (
    mpsc::UnboundedSender<CommandOutputMessage>,
    mpsc::UnboundedReceiver<CommandOutputMessage>,
) {
    mpsc::unbounded_channel()
}

async fn run(cmd: &str, timeout: u64) -> (CommandResult, Vec<CommandOutputMessage>) {
    let (tx, mut rx) = make_channel();
    let cancel = Arc::new(Notify::new());
    let result = run_command(cwd(), inv(cmd, timeout), cancel, tx)
        .await
        .expect("run_command");
    let mut chunks = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        chunks.push(msg);
    }
    (result, chunks)
}

#[tokio::test(flavor = "current_thread")]
async fn exits_zero_with_stdout() {
    let (r, chunks) = run("printf 'hello\\n'", 10).await;
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.termination_reason, CommandTerminationReason::Exited);
    assert_eq!(r.stdout, "hello\n");
    assert!(r.stderr.is_empty());
    assert!(!r.stdout_truncated);
    let stdout_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| c.stream == CommandOutputStream::Stdout)
        .collect();
    assert!(!stdout_chunks.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn non_zero_exit_status_is_carried_in_result() {
    let (r, _) = run("exit 3", 10).await;
    assert_eq!(r.exit_code, Some(3));
    assert_eq!(r.termination_reason, CommandTerminationReason::Exited);
}

#[tokio::test(flavor = "current_thread")]
async fn stderr_is_captured_separately_from_stdout() {
    let (r, chunks) = run("printf 'out'; printf 'err' 1>&2", 10).await;
    assert_eq!(r.stdout, "out");
    assert_eq!(r.stderr, "err");
    let has_stderr = chunks
        .iter()
        .any(|c| c.stream == CommandOutputStream::Stderr);
    assert!(has_stderr);
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_terminates_process_group() {
    let (r, _) = run("sleep 10", 1).await;
    assert_eq!(r.termination_reason, CommandTerminationReason::Timeout);
    // Signal-killed children have `exit_code == None` on this platform.
    assert!(r.exit_code.is_none() || r.exit_code == Some(0));
    // Runtime must be near the timeout, not the 10s sleep.
    assert!(r.duration_ms < 5_000, "duration={}ms", r.duration_ms);
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_terminates_process_group() {
    let (tx, _rx) = make_channel();
    let cancel = Arc::new(Notify::new());
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_clone.notify_one();
    });
    let r = run_command(cwd(), inv("sleep 10", 30), cancel, tx)
        .await
        .expect("run_command");
    assert_eq!(r.termination_reason, CommandTerminationReason::Cancelled);
    assert!(r.duration_ms < 5_000, "duration={}ms", r.duration_ms);
}

#[tokio::test(flavor = "current_thread")]
async fn stdout_limit_is_enforced() {
    // Produce more than COMMAND_MAX_STREAM_BYTES bytes on stdout.
    let cmd = format!("yes hello | head -c {}", COMMAND_MAX_STREAM_BYTES * 2);
    let (r, _) = run(&cmd, 10).await;
    assert_eq!(r.termination_reason, CommandTerminationReason::StdoutLimit);
    assert!(r.stdout_truncated);
    assert!(r.stdout.len() <= COMMAND_MAX_STREAM_BYTES);
}

#[tokio::test(flavor = "current_thread")]
async fn ansi_escapes_are_stripped_from_stdout() {
    let (r, _) = run(r"printf '\033[31mred\033[0m plain'", 10).await;
    assert_eq!(r.stdout, "red plain");
}

#[tokio::test(flavor = "current_thread")]
async fn stdin_is_closed_and_reads_return_eof() {
    // If stdin were inherited or piped, `cat` would hang forever.
    // With Stdio::null(), read hits EOF immediately and cat exits 0.
    let (r, _) = run("cat", 5).await;
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.termination_reason, CommandTerminationReason::Exited);
}

#[tokio::test(flavor = "current_thread")]
async fn nested_child_is_killed_when_group_is_terminated() {
    // The outer shell spawns a background `sleep 30` that would
    // outlive the parent under naive kill. With setsid + killpg the
    // whole group dies on timeout.
    let (r, _) = run("sh -c 'sleep 30 &' ; sleep 30", 1).await;
    assert_eq!(r.termination_reason, CommandTerminationReason::Timeout);
    assert!(r.duration_ms < 5_000, "duration={}ms", r.duration_ms);
}
