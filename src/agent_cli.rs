//! Sync single-turn agent loop for `attini agent`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use nojson::DisplayJson;

use crate::curl::{self, ProgressSinks};
use crate::sansio::agent::{
    CommandInvocation, PatchInvocation, PatchPreview, ReadOnlyTool, ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall, ToolDef};
use crate::session::{
    ApprovalDecision, InvocationEndReason, MetricsSnapshotBody, Pending, PendingToolKind, Session,
    SessionRecord, now_unix_millis,
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

    let tools = build_tool_defs();

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
                    let preview_text = render_command_preview(tc)?;
                    eprintln!("[command] approval required");
                    eprintln!("{preview_text}");
                    save_pending(session, tc, PendingToolKind::Command, preview_text)?;
                    return Ok(Driven::AwaitingApproval);
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

fn build_tool_defs() -> Vec<ToolDef> {
    let mut defs = ReadOnlyTool::definitions();
    defs.push(PatchInvocation::definition());
    defs.push(CommandInvocation::definition());
    defs
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

fn render_command_preview(tc: &ToolCall) -> io::Result<String> {
    let inv = CommandInvocation::parse(&tc.arguments_json)
        .map_err(|e| io::Error::other(format!("command args: {e:?}")))?;
    Ok(format!(
        "command preview: `{}` (timeout {}s)",
        inv.command_line, inv.timeout_seconds
    ))
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

fn run_command_sync(inv: &CommandInvocation, executor: &ToolExecutor) -> io::Result<String> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&inv.command_line)
        .current_dir(executor.root())
        .output()?;
    Ok(command_result_json(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        output.status.code().unwrap_or(-1),
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

fn command_result_json(stdout: &str, stderr: &str, exit_code: i32) -> String {
    struct Payload<'a> {
        stdout: &'a str,
        stderr: &'a str,
        exit_code: i32,
    }
    impl DisplayJson for Payload<'_> {
        fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
            f.object(|f| {
                f.member("stdout", self.stdout)?;
                f.member("stderr", self.stderr)?;
                f.member("exit_code", self.exit_code)
            })
        }
    }
    nojson::Json(Payload {
        stdout,
        stderr,
        exit_code,
    })
    .to_string()
}
