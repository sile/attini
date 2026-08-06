//! Sync single-turn agent loop for `attini agent`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use nojson::DisplayJson;

use crate::curl::{self, ProgressSinks};
use crate::permissions::{self, LoadedRules};
use crate::sansio::agent::{
    CommandError, CommandInvocation, PatchInvocation, PatchPreview, ReadOnlyTool,
    ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall, ToolDef};
use crate::sansio::permissions::{AutoDecision, Judgment, Mode, evaluate};
use crate::session::{
    ApprovalDecision, AutoDecidedBy, ChatMessageWithTs, InvocationEndReason, MetricsSnapshotBody,
    Pending, PendingToolKind, Session, SessionRecord, TokenUsageBody, now_unix_millis,
};
use crate::tools::ToolExecutor;

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_AWAITING_APPROVAL: u8 = 10;

pub const DEFAULT_MAX_TURNS: usize = 20;

/// Counters collected during one invocation of `agent_cli::run` for
/// later persistence into `MetricsSnapshotBody::entries`. Shared by
/// mutable reference between `run()` and `drive()` so both the Ok
/// and Err outcomes flush the same accumulated values.
#[derive(Debug, Default)]
pub struct Counters {
    pub turns: u64,
    pub tool_calls_by_kind: ToolCallsByKind,
    pub tool_errors: u64,
    pub prompt_tokens_billed_total: u64,
    pub completion_tokens_total: u64,
    pub prompt_cache_hit_tokens_total: u64,
    pub prompt_cache_miss_tokens_total: u64,
}

/// Per-tool-name buckets for `Counters::tool_calls_by_kind`. Names are
/// matched directly against `ToolCall::function_name`; anything not
/// in the fixed set falls into `unknown` (mirrors the `Unknown`
/// branch of the dispatch loop's `classify()` helper).
#[derive(Debug, Default)]
pub struct ToolCallsByKind {
    pub list: u64,
    pub read: u64,
    pub search: u64,
    pub patch: u64,
    pub command: u64,
    pub unknown: u64,
}

impl Counters {
    /// Flatten into the `Vec<(String, u64)>` shape expected by
    /// `MetricsSnapshotBody::entries`. Also takes `duration_ms`
    /// separately because that value is known only in `run()`, not
    /// during `drive()`.
    pub fn to_metrics_entries(&self, duration_ms: u64) -> Vec<(String, u64)> {
        vec![
            ("turns".to_string(), self.turns),
            ("tool_calls.list".to_string(), self.tool_calls_by_kind.list),
            ("tool_calls.read".to_string(), self.tool_calls_by_kind.read),
            (
                "tool_calls.search".to_string(),
                self.tool_calls_by_kind.search,
            ),
            (
                "tool_calls.patch".to_string(),
                self.tool_calls_by_kind.patch,
            ),
            (
                "tool_calls.command".to_string(),
                self.tool_calls_by_kind.command,
            ),
            (
                "tool_calls.unknown".to_string(),
                self.tool_calls_by_kind.unknown,
            ),
            ("tool_errors".to_string(), self.tool_errors),
            ("duration_ms".to_string(), duration_ms),
            (
                "prompt_tokens_billed_total".to_string(),
                self.prompt_tokens_billed_total,
            ),
            (
                "completion_tokens_total".to_string(),
                self.completion_tokens_total,
            ),
            (
                "prompt_cache_hit_tokens_total".to_string(),
                self.prompt_cache_hit_tokens_total,
            ),
            (
                "prompt_cache_miss_tokens_total".to_string(),
                self.prompt_cache_miss_tokens_total,
            ),
        ]
    }
}

/// `prompt_tokens` threshold above which the next `Continuation::Prompt`
/// invocation summarises before making the model call. Set to 1/4 of
/// the DeepSeek 64 K context (`64 × 1024 / 4 = 16384`) so compaction
/// leaves room for the next turn's growth plus the memory tier, tool
/// definitions, and the summarizer's own input.
pub const COMPACTION_TRIGGER_TOKENS: u64 = 16_384;

/// Target number of real records to retain past the summary cutoff
/// when compacting. Actual retention may be a little higher: the
/// cutoff snaps toward the tail until it lands on a User record or
/// an Assistant record without pending `tool_calls`, so any pair
/// of `assistant -> tool` records stays together.
pub const KEEP_RECENT_RECORDS_TARGET: usize = 10;

pub struct AgentConfig {
    pub session_name: String,
    pub model: String,
    pub workspace_root: PathBuf,
    pub system_prompt: Option<String>,
    pub show_reasoning: bool,
    pub max_turns: usize,
    pub mode: Mode,
    /// Extra workspace-external read-only path prefixes granted via
    /// `attini agent --read-path`. Combined with the persistent
    /// entries from `permissions.json.extra_read_paths` on startup.
    pub extra_read_paths_cli: Vec<PathBuf>,
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
    // Combine persistent extra_read_paths (from permissions.json) with
    // CLI --read-path overrides for this invocation, canonicalise
    // each, and hand the resulting Vec to the ToolExecutor. Any path
    // that fails to canonicalise is warned + skipped so a single bad
    // entry does not disable the whole grant list.
    let loaded = permissions::load(&cfg.session_name)?;
    let mut candidates: Vec<PathBuf> = loaded.extra_read_paths.iter().map(PathBuf::from).collect();
    candidates.extend(cfg.extra_read_paths_cli.iter().cloned());
    let extra_read_roots = canonicalise_extra_read_roots(&cfg.workspace_root, candidates);
    let executor = ToolExecutor::new(
        &cfg.workspace_root,
        extra_read_roots,
        cfg.session_name.clone(),
    )?;

    let start_ts = now_unix_millis();
    session.append(&SessionRecord::InvocationStart {
        ts: start_ts,
        attini_version: env!("CARGO_PKG_VERSION").to_string(),
        model: cfg.model.clone(),
    })?;

    let mut counters = Counters::default();
    let outcome = drive(&mut session, &executor, &cfg, cont, &mut counters);

    let (reason, exit_code) = match &outcome {
        Ok(Driven::Completed) => (InvocationEndReason::Completed, EXIT_OK),
        Ok(Driven::AwaitingApproval) => (
            InvocationEndReason::AwaitingApproval,
            EXIT_AWAITING_APPROVAL,
        ),
        Err(_) => (InvocationEndReason::Error, EXIT_ERROR),
    };

    let end_ts = now_unix_millis();
    let duration_ms = end_ts.saturating_sub(start_ts);
    let _ = session.append(&SessionRecord::MetricsSnapshot {
        ts: end_ts,
        counters: MetricsSnapshotBody {
            entries: counters.to_metrics_entries(duration_ms),
        },
    });
    let _ = session.append(&SessionRecord::InvocationEnd { ts: end_ts, reason });

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
    counters: &mut Counters,
) -> io::Result<Driven> {
    if matches!(cont, Continuation::Prompt(_)) {
        try_auto_compact(session, &cfg.model)?;
    }

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
            counters.tool_errors += 1;
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
        counters.turns += 1;
        if let Some(usage) = call_result.usage {
            session.append(&SessionRecord::TokenUsage {
                ts: now_unix_millis(),
                body: TokenUsageBody {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    prompt_cache_hit_tokens: usage.prompt_cache_hit_tokens,
                    prompt_cache_miss_tokens: usage.prompt_cache_miss_tokens,
                },
            })?;
            counters.prompt_tokens_billed_total = counters
                .prompt_tokens_billed_total
                .saturating_add(usage.prompt_tokens.unwrap_or(0));
            counters.completion_tokens_total = counters
                .completion_tokens_total
                .saturating_add(usage.completion_tokens.unwrap_or(0));
            counters.prompt_cache_hit_tokens_total = counters
                .prompt_cache_hit_tokens_total
                .saturating_add(usage.prompt_cache_hit_tokens.unwrap_or(0));
            counters.prompt_cache_miss_tokens_total = counters
                .prompt_cache_miss_tokens_total
                .saturating_add(usage.prompt_cache_miss_tokens.unwrap_or(0));
        }
        messages.push(assistant);

        if call_result.tool_calls.is_empty() {
            return Ok(Driven::Completed);
        }

        for tc in &call_result.tool_calls {
            // Fine-grained tool_calls_by_kind counting is done here
            // (not via classify()) because classify() collapses
            // list/read/search into ToolKind::ReadOnly for dispatch.
            match tc.function_name.as_str() {
                "list" => counters.tool_calls_by_kind.list += 1,
                "read" => counters.tool_calls_by_kind.read += 1,
                "search" => counters.tool_calls_by_kind.search += 1,
                "patch" => counters.tool_calls_by_kind.patch += 1,
                "command" => counters.tool_calls_by_kind.command += 1,
                _ => counters.tool_calls_by_kind.unknown += 1,
            }
            match classify(&tc.function_name) {
                ToolKind::ReadOnly => {
                    let (summary, content, errored) = run_read_only(tc, executor);
                    eprintln!("{summary}");
                    if errored {
                        counters.tool_errors += 1;
                    }
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
                    match dispatch_command(
                        tc,
                        executor,
                        cfg.mode,
                        &rules,
                        session,
                        &mut messages,
                        counters,
                    )? {
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
                    counters.tool_errors += 1;
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

fn canonicalise_extra_read_roots(
    workspace_root: &std::path::Path,
    candidates: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut seen = std::collections::BTreeSet::<PathBuf>::new();
    let mut out = Vec::new();
    for p in candidates {
        let absolute = if p.is_absolute() {
            p.clone()
        } else {
            workspace_root.join(&p)
        };
        match absolute.canonicalize() {
            Ok(canon) => {
                if seen.insert(canon.clone()) {
                    out.push(canon);
                }
            }
            Err(e) => eprintln!("attini: extra_read_paths: skipping {}: {e}", p.display()),
        }
    }
    out
}

fn build_initial_messages(session: &Session, cfg: &AgentConfig) -> io::Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    if let Some(mem) = crate::memories::load(&cfg.session_name)? {
        messages.push(ChatMessage::System(mem));
    }
    let summaries = session.load_summaries()?;
    let total = summaries.len();
    for (i, summary) in summaries.into_iter().enumerate() {
        let header = if total > 1 {
            format!(
                "# Prior conversation summary (part {} of {})\n\n",
                i + 1,
                total
            )
        } else {
            "# Prior conversation summary\n\n".to_string()
        };
        messages.push(ChatMessage::System(format!("{header}{}", summary.text)));
    }
    if let Some(sys) = &cfg.system_prompt {
        messages.push(ChatMessage::System(sys.clone()));
    }
    for record in session.load_records_since_last_summary()? {
        messages.push(record.message);
    }
    Ok(messages)
}

// -------------------------------------------------------------------
// Compaction: summarise older records and append a `summary` record
// -------------------------------------------------------------------

const SUMMARIZER_SYSTEM_PROMPT: &str = "You are summarizing a conversation between a user and a coding agent \
so the agent can continue with a shorter context. Preserve:\n\
\n\
- Unfinished tasks and any next steps the user or agent laid out\n\
- Decisions reached (chosen approaches; rejected alternatives with the reason)\n\
- File paths and key symbols (functions, types) that were read, modified,\n\
  or discussed\n\
- Recent errors and their root cause, if any\n\
\n\
Aim for ~500 words of plain prose. Do not include markdown code fences \
unless quoting a short critical excerpt. Do not comment on the \
summarization itself; produce only the summary.";

fn try_auto_compact(session: &mut Session, model: &str) -> io::Result<()> {
    if session.load_pending()?.is_some() {
        return Ok(());
    }
    let Some(latest) = session.latest_prompt_tokens()? else {
        return Ok(());
    };
    if latest < COMPACTION_TRIGGER_TOKENS {
        return Ok(());
    }
    eprintln!(
        "[compaction] previous prompt was {latest} tokens (threshold {COMPACTION_TRIGGER_TOKENS}), summarising..."
    );
    if let Err(e) = compact_conversation(session, model) {
        eprintln!("[compaction] failed, continuing with full history: {e}");
    }
    Ok(())
}

/// Run one compaction pass against `session`. Reads real records
/// since the last summary, picks a safe cutoff so no
/// `assistant -> tool` pair is split, sends the older records to
/// the summarizer, and appends a `SessionRecord::Summary`.
///
/// Exposed to `session_cmd` for the manual `attini session compact`
/// subcommand. Callers are expected to have already checked that
/// the session is idle (no LOCK holder, no `pending.json`).
pub fn compact_conversation(session: &mut Session, model: &str) -> io::Result<()> {
    let records = session.load_records_since_last_summary()?;
    let keep_start = safe_tail_start(&records, KEEP_RECENT_RECORDS_TARGET);
    if keep_start == 0 {
        eprintln!("[compaction] no records eligible for summarisation. skipping.");
        return Ok(());
    }
    let to_summarise: Vec<ChatMessageWithTs> = records[..keep_start].to_vec();
    let record_count = to_summarise.len();
    let since_ts = to_summarise
        .first()
        .map(|r| r.ts)
        .expect("keep_start > 0 so to_summarise is non-empty");
    let cutoff_ts = to_summarise
        .last()
        .map(|r| r.ts)
        .expect("keep_start > 0 so to_summarise is non-empty");

    let text = run_summariser(model, to_summarise)?;
    let words = text.split_whitespace().count();

    session.append(&SessionRecord::Summary {
        ts: now_unix_millis(),
        since_ts,
        cutoff_ts,
        text,
    })?;
    eprintln!("[compaction] applied. summarised {record_count} records into ~{words} words.");
    Ok(())
}

fn run_summariser(model: &str, records: Vec<ChatMessageWithTs>) -> io::Result<String> {
    let mut messages = vec![ChatMessage::System(SUMMARIZER_SYSTEM_PROMPT.to_string())];
    messages.extend(records.into_iter().map(|r| r.message));
    let request = ChatRequest::new(model.to_string(), messages);
    let mut sink = io::sink();
    let mut sinks = ProgressSinks {
        content: &mut sink,
        reasoning: None,
    };
    let result = curl::call(&request, &mut sinks)
        .map_err(|e| io::Error::other(format!("summariser call failed: {e}")))?;
    if result.content.trim().is_empty() {
        return Err(io::Error::other("summariser returned empty content"));
    }
    Ok(result.content)
}

/// Given the real records that follow the last summary, return
/// the index from which the tail is kept intact. Records at
/// smaller indices are candidates for the new summary.
///
/// The cutoff never falls inside an `assistant -> tool` pair: it
/// snaps toward the tail until it lands on a User record or an
/// Assistant record without `tool_calls`. Returns `records.len()`
/// (kept = nothing) if no safe boundary exists past the initial
/// target — which happens when the tail is a single unresolved
/// `assistant -> tool` pair, in which case leaving everything as
/// candidates would still be wrong, so we bail and keep the whole
/// tail by returning 0 as well.
fn safe_tail_start(records: &[ChatMessageWithTs], target_keep: usize) -> usize {
    let n = records.len();
    if n <= target_keep {
        return 0;
    }
    let mut i = n - target_keep;
    while i < n {
        if is_safe_boundary(&records[i].message) {
            return i;
        }
        i += 1;
    }
    // No safe boundary in the tail — refuse to summarise anything
    // this round rather than emit an orphan tool message.
    0
}

fn is_safe_boundary(msg: &ChatMessage) -> bool {
    match msg {
        ChatMessage::User(_) => true,
        ChatMessage::Assistant { tool_calls, .. } => tool_calls.is_empty(),
        _ => false,
    }
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
    counters: &mut Counters,
) -> io::Result<CommandDispatch> {
    let inv = match CommandInvocation::parse(&tc.arguments_json) {
        Ok(inv) => inv,
        Err(err) => {
            let content = tool_error_json("command_args", &format!("{err:?}"));
            eprintln!("[command] parse err: {err:?}");
            counters.tool_errors += 1;
            append_tool(session, messages, &tc.id, content)?;
            return Ok(CommandDispatch::Continue);
        }
    };
    let judgment = evaluate(mode, &rules.session, &rules.workspace, &inv.argv);
    let display = shell_escape_argv(&inv.argv);
    match judgment {
        Judgment::AutoApprove(dec) => {
            let dec_display = shell_escape_argv(&dec.argv_prefix);
            eprintln!(
                "[command] auto-approve via {} rule '{}': {}",
                dec.scope.as_str(),
                dec_display,
                display
            );
            append_auto_approval(session, &tc.id, ApprovalDecision::Approve, &dec)?;
            let content = match run_command_sync(&inv, executor) {
                Ok(s) => s,
                Err(err) => {
                    let (code, msg) = err.to_code_and_message();
                    counters.tool_errors += 1;
                    let payload = tool_error_json(code, &msg);
                    append_tool(session, messages, &tc.id, payload)?;
                    return Ok(CommandDispatch::Continue);
                }
            };
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::AutoDeny(dec) => {
            let dec_display = shell_escape_argv(&dec.argv_prefix);
            eprintln!(
                "[command] auto-deny via {} rule '{}': {}",
                dec.scope.as_str(),
                dec_display,
                display
            );
            append_auto_approval(session, &tc.id, ApprovalDecision::Reject, &dec)?;
            let content = tool_error_json(
                "denied_by_rule",
                &format!(
                    "auto-denied by {} rule argv_prefix {:?}",
                    dec.scope.as_str(),
                    dec.argv_prefix
                ),
            );
            counters.tool_errors += 1;
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::PlanReject { reason } => {
            eprintln!(
                "[command] plan_mode reject ({}): {}",
                reason.as_str(),
                display
            );
            let sidecar = AutoDecidedBy {
                scope: "plan".to_string(),
                argv_prefix: Vec::new(),
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
            counters.tool_errors += 1;
            append_tool(session, messages, &tc.id, content)?;
            Ok(CommandDispatch::Continue)
        }
        Judgment::Pending => {
            let preview_text = render_command_preview_from(&inv);
            eprintln!("[command] approval required");
            eprintln!("{preview_text}");
            emit_suggested_rule(&inv.argv);
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
        argv_prefix: dec.argv_prefix.clone(),
        reason: dec.reason.as_str().to_string(),
    };
    session.append(&SessionRecord::ToolApproval {
        ts: now_unix_millis(),
        call_id: call_id.to_string(),
        decision,
        auto_decided_by: Some(sidecar),
    })
}

/// Suggest the two `attini session grant` invocations that would
/// pre-approve the argv-prefix of the pending command. `argv` is
/// truncated to at most two elements (typical pattern: `program
/// subcommand`) so the rule stays a general prefix rather than
/// baking every flag in.
fn emit_suggested_rule(argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    let take = argv.len().min(2);
    let prefix_display = shell_escape_argv(&argv[..take]);
    eprintln!("suggested rule (persist separately after approve):");
    eprintln!("  attini session grant {prefix_display}                # session-local");
    eprintln!("  attini session grant {prefix_display} --workspace    # workspace-wide");
}

/// Unconditionally wrap `s` in POSIX single-quotes, escaping any
/// interior single-quotes with the `'\\''` sequence. Used by
/// [`shell_escape_argv`] as the quoting primitive.
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

/// Format an argv slice as a shell-escaped single-line command for
/// human display (preview text, suggested-rule hint, log lines).
/// Simple tokens are emitted raw; empty strings and tokens
/// containing whitespace or POSIX shell metacharacters are
/// single-quoted. The output is not eval-safe in every corner but is
/// unambiguous for the argv patterns coding agents typically
/// produce.
fn shell_escape_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|s| {
            if s.is_empty() || s.chars().any(needs_shell_quote) {
                shell_single_quote(s)
            } else {
                s.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn needs_shell_quote(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '|'
            | '&'
            | ';'
            | '('
            | ')'
            | '$'
            | '`'
            | '>'
            | '<'
            | '\\'
            | '"'
            | '\''
            | '*'
            | '?'
            | '['
            | ']'
            | '{'
            | '}'
            | '!'
            | '#'
            | '~'
            | '='
    )
}

fn render_command_preview_from(inv: &CommandInvocation) -> String {
    format!("command preview: {}", shell_escape_argv(&inv.argv))
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

/// Returns `(display_summary, tool_response_content, errored)`.
/// `errored` is `true` iff the returned content is a `tool_error_json`
/// payload (parse failure or executor error) so the caller can update
/// `Counters::tool_errors` without re-parsing the string.
fn run_read_only(tc: &ToolCall, executor: &ToolExecutor) -> (String, String, bool) {
    match ReadOnlyTool::parse(&tc.function_name, &tc.arguments_json) {
        Ok(inv) => {
            let args_summary = summarize_read_only(&inv);
            match executor.execute(inv) {
                ToolOutcome::Ok(payload) => (format!("[{args_summary}] ok"), payload, false),
                ToolOutcome::Err(err) => (
                    format!("[{args_summary}] err: {}", short_err(&err)),
                    tool_error_json_from(&err),
                    true,
                ),
            }
        }
        Err(err) => (
            format!("[{}] parse err: {}", tc.function_name, short_err(&err)),
            tool_error_json_from(&err),
            true,
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
            match run_command_sync(&inv, executor) {
                Ok(s) => Ok(s),
                Err(err) => {
                    let (code, msg) = err.to_code_and_message();
                    Ok(tool_error_json(code, &msg))
                }
            }
        }
    }
}

fn run_command_sync(
    inv: &CommandInvocation,
    executor: &ToolExecutor,
) -> Result<String, CommandError> {
    let started = Instant::now();
    // argv is guaranteed non-empty by CommandInvocation::parse.
    let output = Command::new(&inv.argv[0])
        .args(&inv.argv[1..])
        .current_dir(executor.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CommandError::SpawnFailed {
            message: e.to_string(),
        })?;
    let elapsed = started.elapsed();
    let termination_reason = if output.status.code().is_some() {
        "exited"
    } else {
        "signaled"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(ts: u64) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::User(format!("u{ts}")),
            ts,
        }
    }

    fn assistant_plain(ts: u64) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::assistant_text(format!("a{ts}")),
            ts,
        }
    }

    fn assistant_with_tool_call(ts: u64) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::Assistant {
                content: String::new(),
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: format!("call_{ts}"),
                    function_name: "read".to_string(),
                    arguments_json: "{}".to_string(),
                }],
            },
            ts,
        }
    }

    fn tool(ts: u64) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::Tool {
                tool_call_id: format!("call_{ts}"),
                content: "{}".to_string(),
            },
            ts,
        }
    }

    #[test]
    fn safe_tail_start_returns_zero_when_short_history() {
        let records = vec![user(1), assistant_plain(2)];
        assert_eq!(safe_tail_start(&records, 10), 0);
    }

    #[test]
    fn safe_tail_start_lands_on_user_record() {
        // Target keep = 2 → naive cutoff at index 3 (assistant plain).
        // That is already a safe boundary, so cutoff stays.
        let records = vec![
            user(1),
            assistant_plain(2),
            user(3),
            assistant_plain(4),
            user(5),
        ];
        assert_eq!(safe_tail_start(&records, 2), 3);
    }

    #[test]
    fn safe_tail_start_advances_past_tool_record() {
        // Target keep = 2 → naive cutoff at index 3 (tool), which is
        // unsafe because it would orphan the tool from its assistant.
        // The safe cutoff is the next user record at index 4.
        let records = vec![
            user(1),
            assistant_plain(2),
            assistant_with_tool_call(3),
            tool(4),
            user(5),
        ];
        assert_eq!(safe_tail_start(&records, 2), 4);
    }

    #[test]
    fn safe_tail_start_advances_past_assistant_with_tool_calls() {
        // Target keep = 2 → naive cutoff at index 2 (assistant with
        // tool_calls). Cutting there would drop the tool_call context
        // but keep the tool response, so it is unsafe. Move forward
        // to the next safe boundary at index 4 (user).
        let records = vec![
            user(1),
            user(2),
            assistant_with_tool_call(3),
            tool(4),
            user(5),
        ];
        assert_eq!(safe_tail_start(&records, 3), 4);
    }

    #[test]
    fn safe_tail_start_bails_when_no_safe_boundary_in_tail() {
        // Tail is a single unresolved assistant→tool pair; no safe
        // boundary between naive index (1) and end. Bail with 0 so
        // we do not orphan tools this round.
        let records = vec![user(1), assistant_with_tool_call(2), tool(3)];
        assert_eq!(safe_tail_start(&records, 1), 0);
    }

    #[test]
    fn is_safe_boundary_classifies_records_as_expected() {
        assert!(is_safe_boundary(&ChatMessage::User("u".to_string())));
        assert!(is_safe_boundary(&ChatMessage::assistant_text("a")));
        assert!(!is_safe_boundary(&ChatMessage::Assistant {
            content: String::new(),
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "x".to_string(),
                function_name: "read".to_string(),
                arguments_json: "{}".to_string(),
            }],
        }));
        assert!(!is_safe_boundary(&ChatMessage::Tool {
            tool_call_id: "x".to_string(),
            content: "{}".to_string(),
        }));
        assert!(!is_safe_boundary(&ChatMessage::System("s".to_string())));
    }
}
