//! Sync single-turn agent loop for `attini agent`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nojson::DisplayJson;

use crate::curl::{self, ProgressSinks};
use crate::permissions::{self, LoadedRules};
use crate::sansio::agent::{
    CommandInvocation, PatchInvocation, PatchPreview, ReadOnlyTool, ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall, ToolDef};
use crate::sansio::permissions::{
    AutoDecision, Judgment, Mode, evaluate, has_shell_operator, tokenize,
};
use crate::session::{
    ApprovalDecision, AutoDecidedBy, InvocationEndReason, MetricsSnapshotBody, Pending,
    PendingToolKind, Session, SessionRecord, now_unix_millis,
};
use crate::tools::ToolExecutor;

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_AWAITING_APPROVAL: u8 = 10;

pub const DEFAULT_MAX_TURNS: usize = 20;

pub struct AgentConfig {
    pub session_name: String,
    pub model: String,
    pub workspace_root: PathBuf,
    pub system_prompt: Option<String>,
    pub show_reasoning: bool,
    pub max_turns: usize,
    pub mode: Mode,
}

pub enum Continuation {
    /// A fresh user prompt for this invocation.
    Prompt(String),
    /// Resume by approving the pending tool call recorded in the
    /// session's `pending.json`.
    Approve,
    /// Resume by rejecting the pending tool call.
    Reject,
}

pub fn run(cfg: AgentConfig, cont: Continuation) -> io::Result<ExitCode> {
    let mut session = Session::open(&cfg.session_name)?;
    let executor = ToolExecutor::new(&cfg.workspace_root)?;

    session.append(&SessionRecord::InvocationStart {
        ts: now_unix_millis(),
        attini_version: env!("CARGO_PKG_VERSION").to_string(),
        model: cfg.model.clone(),
    })?;

    let outcome = drive(&mut session, &executor, &cfg, cont);

    let (reason, exit_code) = match &outcome {
        Ok(Driven::Completed) => (InvocationEndReason::Completed, EXIT_OK),
        Ok(Driven::AwaitingApproval) => (
            InvocationEndReason::AwaitingApproval,
            EXIT_AWAITING_APPROVAL,
        ),
        Err(_) => (InvocationEndReason::Error, EXIT_ERROR),
    };

    let _ = session.append(&SessionRecord::MetricsSnapshot {
        ts: now_unix_millis(),
        counters: MetricsSnapshotBody::default(),
    });
    let _ = session.append(&SessionRecord::InvocationEnd {
        ts: now_unix_millis(),
        reason,
    });

    match outcome {
        Ok(_) => Ok(ExitCode::from(exit_code)),
        Err(e) => {
            eprintln!("attini: {e}");
            Ok(ExitCode::from(EXIT_ERROR))
        }
    }
}

enum Driven {
    Completed,
    AwaitingApproval,
}

fn drive(
    session: &mut Session,
    executor: &ToolExecutor,
    cfg: &AgentConfig,
    cont: Continuation,
) -> io::Result<Driven> {
    let mut messages = build_initial_messages(session, cfg)?;

    match cont {
        Continuation::Prompt(text) => {
            session.append(&SessionRecord::User {
                ts: now_unix_millis(),
                text: text.clone(),
            })?;
            messages.push(ChatMessage::User(text));
        }
        Continuation::Approve => {
            let pending = load_pending_or_err(session)?;
            session.append(&SessionRecord::ToolApproval {
                ts: now_unix_millis(),
                call_id: pending.call_id.clone(),
                decision: ApprovalDecision::Approve,
                auto_decided_by: None,
            })?;
            let content = execute_pending(&pending, executor)?;
            append_tool(session, &mut messages, &pending.call_id, content)?;
            session.clear_pending()?;
        }
        Continuation::Reject => {
            let pending = load_pending_or_err(session)?;
            session.append(&SessionRecord::ToolApproval {
                ts: now_unix_millis(),
                call_id: pending.call_id.clone(),
                decision: ApprovalDecision::Reject,
                auto_decided_by: None,
            })?;
            let content = r#"{"error":"rejected","message":"user rejected this tool call"}"#;
            append_tool(
                session,
                &mut messages,
                &pending.call_id,
                content.to_string(),
            )?;
            session.clear_pending()?;
        }
    }

    let tools = build_tool_defs(cfg.mode);
    let rules = permissions::load(&cfg.session_name)?;

    for _ in 0..cfg.max_turns {
        let request =
            ChatRequest::new(cfg.model.clone(), messages.clone()).with_tools(tools.clone());
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        let call_result = {
            let mut sinks = ProgressSinks {
                content: &mut stdout,
                reasoning: if cfg.show_reasoning {
                    Some(&mut stderr)
                } else {
                    None
                },
            };
            curl::call(&request, &mut sinks)
                .map_err(|e| io::Error::other(format!("model call failed: {e}")))?
        };
        let _ = writeln!(io::stdout());

        let assistant = call_result.clone().into_assistant();
        session.append(&SessionRecord::Assistant {
            ts: now_unix_millis(),
            content: call_result.content.clone(),
            reasoning: call_result.reasoning_content.clone(),
            tool_calls: call_result.tool_calls.clone(),
        })?;
        messages.push(assistant);

        if call_result.tool_calls.is_empty() {
            return Ok(Driven::Completed);
        }

        for tc in &call_result.tool_calls {
            match classify(&tc.function_name) {
                ToolKind::ReadOnly => {
                    let (summary, content) = run_read_only(tc, executor);
                    eprintln!("{summary}");
                    append_tool(session, &mut messages, &tc.id, content)?;
                }
                ToolKind::Patch => {
                    let preview_text = render_patch_preview(tc, executor)?;
                    eprintln!("[patch] approval required");
                    eprintln!("{preview_text}");
                    save_pending(session, tc, PendingToolKind::Patch, preview_text)?;
                    return Ok(Driven::AwaitingApproval);
                }
                ToolKind::Command => {
                    match dispatch_command(tc, executor, cfg.mode, &rules, session, &mut messages)?
                    {
                        CommandDispatch::Awaiting => return Ok(Driven::AwaitingApproval),
                        CommandDispatch::Continue => {}
                    }
                }
                ToolKind::Unknown => {
                    let content = tool_error_json(
                        "unknown_tool",
                        &format!("no such tool: {}", tc.function_name),
                    );
                    eprintln!("[unknown tool] {}", tc.function_name);
                    append_tool(session, &mut messages, &tc.id, content)?;
                }
            }
        }
    }

    Err(io::Error::other(format!(
        "agent loop exceeded max_turns={}",
        cfg.max_turns
    )))
}

fn build_initial_messages(session: &Session, cfg: &AgentConfig) -> io::Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    if let Some(sys) = &cfg.system_prompt {
        messages.push(ChatMessage::System(sys.clone()));
    }
    messages.extend(session.load_conversation()?);
    Ok(messages)
}

fn build_tool_defs(mode: Mode) -> Vec<ToolDef> {
    let mut defs = ReadOnlyTool::definitions();
    if !matches!(mode, Mode::Plan) {
        defs.push(PatchInvocation::definition());
    }
    defs.push(CommandInvocation::definition());
    defs
}

enum CommandDispatch {
    Awaiting,
    Continue,
}

fn dispatch_command(
    tc: &ToolCall,
    executor: &ToolExecutor,
    mode: Mode,
    rules: &LoadedRules,
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
) -> io::Result<CommandDispatch> {
    let inv = match CommandInvocation::parse(&tc.arguments_json) {
        Ok(inv) => inv,
        Err(err) => {
            let content = tool_error_json("command_args", &format!("{err:?}"));
            eprintln!("[command] parse err: {err:?}");
            append_tool(session, messages, &tc.id, content)?;
            return Ok(CommandDispatch::Continue);
        }
    };
    let judgment = evaluate(mode, &rules.session, &rules.workspace, &inv.command_line);
    match judgment {
        Judgment::AutoApprove(dec) => {
            eprintln!(
                "[command] auto-approve via {} rule '{}': {}",
                dec.scope.as_str(),
                dec.prefix,
                inv.command_line
            );
            append_auto_approval(session, &tc.id, ApprovalDecision::Approve, &dec)?;
            let content = run_command_sync(&inv, executor)?;
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::AutoDeny(dec) => {
            eprintln!(
                "[command] auto-deny via {} rule '{}': {}",
                dec.scope.as_str(),
                dec.prefix,
                inv.command_line
            );
            append_auto_approval(session, &tc.id, ApprovalDecision::Reject, &dec)?;
            let content = tool_error_json(
                "denied_by_rule",
                &format!(
                    "auto-denied by {} rule prefix '{}'",
                    dec.scope.as_str(),
                    dec.prefix
                ),
            );
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::PlanReject { reason } => {
            eprintln!(
                "[command] plan_mode reject ({}): {}",
                reason.as_str(),
                inv.command_line
            );
            let sidecar = AutoDecidedBy {
                scope: "plan".to_string(),
                prefix: String::new(),
                reason: format!("plan_mode_reject:{}", reason.as_str()),
            };
            session.append(&SessionRecord::ToolApproval {
                ts: now_unix_millis(),
                call_id: tc.id.clone(),
                decision: ApprovalDecision::Reject,
                auto_decided_by: Some(sidecar),
            })?;
            let content = tool_error_json(
                "plan_mode",
                &format!(
                    "plan mode: command rejected ({}). drop --plan to run manually.",
                    reason.as_str()
                ),
            );
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::Pending => {
            let preview_text = render_command_preview_from(&inv);
            let hit_safety = has_shell_operator(&inv.command_line);
            eprintln!("[command] approval required");
            eprintln!("{preview_text}");
            emit_suggested_rule(&inv.command_line, hit_safety);
            save_pending(session, tc, PendingToolKind::Command, preview_text)?;
            Ok(CommandDispatch::Awaiting)
        }
    }
}

fn append_auto_approval(
    session: &mut Session,
    call_id: &str,
    decision: ApprovalDecision,
    dec: &AutoDecision,
) -> io::Result<()> {
    let sidecar = AutoDecidedBy {
        scope: dec.scope.as_str().to_string(),
        prefix: dec.prefix.clone(),
        reason: dec.reason.as_str().to_string(),
    };
    session.append(&SessionRecord::ToolApproval {
        ts: now_unix_millis(),
        call_id: call_id.to_string(),
        decision,
        auto_decided_by: Some(sidecar),
    })
}

fn emit_suggested_rule(command_line: &str, hit_safety: bool) {
    if hit_safety {
        eprintln!(
            "note: contains shell operator; cannot be pre-approved via `attini session grant`."
        );
        return;
    }
    let tokens = tokenize(command_line);
    if tokens.is_empty() {
        return;
    }
    let take = tokens.len().min(2);
    let prefix_display = tokens[..take]
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let quoted = shell_single_quote(&prefix_display);
    eprintln!("suggested rule (persist separately after approve):");
    eprintln!("  attini session grant {quoted}                # session-local");
    eprintln!("  attini session grant {quoted} --workspace    # workspace-wide");
}

fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn render_command_preview_from(inv: &CommandInvocation) -> String {
    format!(
        "command preview: `{}` (timeout {}s)",
        inv.command_line, inv.timeout_seconds
    )
}

enum ToolKind {
    ReadOnly,
    Patch,
    Command,
    Unknown,
}

fn classify(name: &str) -> ToolKind {
    match name {
        "list" | "read" | "search" => ToolKind::ReadOnly,
        "patch" => ToolKind::Patch,
        "command" => ToolKind::Command,
        _ => ToolKind::Unknown,
    }
}

fn run_read_only(tc: &ToolCall, executor: &ToolExecutor) -> (String, String) {
    match ReadOnlyTool::parse(&tc.function_name, &tc.arguments_json) {
        Ok(inv) => {
            let args_summary = summarize_read_only(&inv);
            match executor.execute(inv) {
                ToolOutcome::Ok(payload) => (format!("[{args_summary}] ok"), payload),
                ToolOutcome::Err(err) => (
                    format!("[{args_summary}] err: {}", short_err(&err)),
                    tool_error_json_from(&err),
                ),
            }
        }
        Err(err) => (
            format!("[{}] parse err: {}", tc.function_name, short_err(&err)),
            tool_error_json_from(&err),
        ),
    }
}

fn summarize_read_only(inv: &ReadOnlyTool) -> String {
    match inv {
        ReadOnlyTool::List {
            path, recursive, ..
        } => {
            if *recursive {
                format!(r#"list "{path}" recursive"#)
            } else {
                format!(r#"list "{path}""#)
            }
        }
        ReadOnlyTool::Read { path, line_range } => match line_range {
            Some((start, end)) => format!(r#"read "{path}" lines {start}..{end}"#),
            None => format!(r#"read "{path}""#),
        },
        ReadOnlyTool::Search {
            pattern,
            path_prefix,
            ..
        } => match path_prefix {
            Some(prefix) => format!(r#"search "{pattern}" in "{prefix}""#),
            None => format!(r#"search "{pattern}""#),
        },
    }
}

fn short_err(err: &ToolExecutionError) -> String {
    let full = format!("{err:?}");
    if let Some(idx) = full.find(['(', ' ']) {
        full[..idx].to_string()
    } else {
        full
    }
}

fn render_patch_preview(tc: &ToolCall, executor: &ToolExecutor) -> io::Result<String> {
    let inv = PatchInvocation::parse(&tc.arguments_json)
        .map_err(|e| io::Error::other(format!("patch args: {e:?}")))?;
    let (_, preview) = executor
        .preview_patch(&inv)
        .map_err(|e| io::Error::other(format!("patch preview: {e:?}")))?;
    Ok(render_patch_preview_text(&preview))
}

fn render_patch_preview_text(p: &PatchPreview) -> String {
    let mut out = format!(
        "patch preview: {} edit(s) across {} file(s), +{} / -{} lines",
        p.edit_count,
        p.target_paths.len(),
        p.added_lines,
        p.removed_lines
    );
    for path in &p.target_paths {
        out.push_str("\n  ");
        out.push_str(path);
    }
    out
}

fn save_pending(
    session: &Session,
    tc: &ToolCall,
    kind: PendingToolKind,
    preview: String,
) -> io::Result<()> {
    session.save_pending(&Pending {
        ts: now_unix_millis(),
        call_id: tc.id.clone(),
        tool_kind: kind,
        function_name: tc.function_name.clone(),
        arguments_json: tc.arguments_json.clone(),
        preview,
    })
}

fn execute_pending(pending: &Pending, executor: &ToolExecutor) -> io::Result<String> {
    match pending.tool_kind {
        PendingToolKind::Patch => {
            let inv = PatchInvocation::parse(&pending.arguments_json)
                .map_err(|e| io::Error::other(format!("patch args: {e:?}")))?;
            let (hashes, _preview) = executor
                .preview_patch(&inv)
                .map_err(|e| io::Error::other(format!("patch preview: {e:?}")))?;
            let paths = executor
                .apply_patch(&inv, &hashes)
                .map_err(|e| io::Error::other(format!("patch apply: {e:?}")))?;
            Ok(patch_result_json(&paths))
        }
        PendingToolKind::Command => {
            let inv = CommandInvocation::parse(&pending.arguments_json)
                .map_err(|e| io::Error::other(format!("command args: {e:?}")))?;
            run_command_sync(&inv, executor)
        }
    }
}

const COMMAND_KILL_GRACE_MS: u64 = 500;

fn run_command_sync(inv: &CommandInvocation, executor: &ToolExecutor) -> io::Result<String> {
    let started = Instant::now();
    let timeout = Duration::from_secs(inv.timeout_seconds);
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&inv.command_line)
        .current_dir(executor.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let child_id = child.id() as libc::pid_t;

    let done = Arc::new(AtomicBool::new(false));
    let done_for_killer = done.clone();
    thread::spawn(move || {
        thread::sleep(timeout);
        if done_for_killer.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: `kill` with SIGTERM/SIGKILL to a pid we spawned; no
        // memory invariants at play.
        unsafe { libc::kill(child_id, libc::SIGTERM) };
        thread::sleep(Duration::from_millis(COMMAND_KILL_GRACE_MS));
        if done_for_killer.load(Ordering::Relaxed) {
            return;
        }
        unsafe { libc::kill(child_id, libc::SIGKILL) };
    });

    let output = child.wait_with_output()?;
    done.store(true, Ordering::Relaxed);
    let elapsed = started.elapsed();
    let termination_reason = if elapsed >= timeout || output.status.code().is_none() {
        "timeout"
    } else {
        "exited"
    };
    Ok(command_result_json(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        output.status.code(),
        termination_reason,
        elapsed,
    ))
}

fn append_tool(
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
    call_id: &str,
    content: String,
) -> io::Result<()> {
    session.append(&SessionRecord::Tool {
        ts: now_unix_millis(),
        call_id: call_id.to_string(),
        content: content.clone(),
    })?;
    messages.push(ChatMessage::Tool {
        tool_call_id: call_id.to_string(),
        content,
    });
    Ok(())
}

fn load_pending_or_err(session: &Session) -> io::Result<Pending> {
    session
        .load_pending()?
        .ok_or_else(|| io::Error::other("no pending.json — nothing to approve or reject"))
}

fn tool_error_json_from(err: &ToolExecutionError) -> String {
    tool_error_json("execution_error", &format!("{err:?}"))
}

fn tool_error_json(code: &str, message: &str) -> String {
    struct Payload<'a> {
        code: &'a str,
        message: &'a str,
    }
    impl DisplayJson for Payload<'_> {
        fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
            f.object(|f| {
                f.member("error", self.code)?;
                f.member("message", self.message)
            })
        }
    }
    nojson::Json(Payload { code, message }).to_string()
}

fn patch_result_json(applied: &[PathBuf]) -> String {
    struct Payload<'a> {
        applied: &'a [PathBuf],
    }
    impl DisplayJson for Payload<'_> {
        fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
            f.object(|f| {
                f.member("ok", true)?;
                f.member(
                    "applied",
                    self.applied
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>(),
                )
            })
        }
    }
    nojson::Json(Payload { applied }).to_string()
}

fn command_result_json(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    termination_reason: &str,
    elapsed: Duration,
) -> String {
    struct Payload<'a> {
        stdout: &'a str,
        stderr: &'a str,
        exit_code: Option<i32>,
        termination_reason: &'a str,
        duration_ms: u64,
    }
    impl DisplayJson for Payload<'_> {
        fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
            f.object(|f| {
                match self.exit_code {
                    Some(code) => f.member("exit_code", code)?,
                    None => f.member("exit_code", Option::<i32>::None)?,
                }
                f.member("termination_reason", self.termination_reason)?;
                f.member("duration_ms", self.duration_ms)?;
                f.member("stdout", self.stdout)?;
                f.member("stderr", self.stderr)
            })
        }
    }
    let duration_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
    nojson::Json(Payload {
        stdout,
        stderr,
        exit_code,
        termination_reason,
        duration_ms,
    })
    .to_string()
}
