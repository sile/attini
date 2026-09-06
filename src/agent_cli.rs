//! Sync single-turn agent loop for `attini agent`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use nojson::DisplayJson;

use crate::curl::{self, ProgressSinks};
use crate::permissions::{self, LoadedRules};
use crate::sansio::agent::{
    CommandError, CommandInvocation, PatchInvocation, PatchPreview, ReadOnlyTool,
    SkillLoadInvocation, ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall, ToolDef};
use crate::sansio::permissions::{Authorization, AutoDecision, Judgment, Mode, evaluate};
use crate::session::{
    ApprovalDecision, AutoDecidedBy, ChatMessageWithTs, InvocationEndReason, MetricsSnapshotBody,
    Pending, PendingToolKind, Session, SessionRecord, TokenUsageBody, now_unix_millis,
};
use crate::skills::{self, SkillEntry};
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
    /// Latest per-call `prompt_tokens` from the most recent successful
    /// model call. Unlike [`Counters::prompt_tokens_billed_total`]
    /// (a cumulative sum over the invocation), this reflects the *current*
    /// conversation size and is what the status line's `ctx=` shows.
    pub prompt_tokens_last: u64,
    pub completion_tokens_total: u64,
    pub prompt_cache_hit_tokens_total: u64,
    pub prompt_cache_miss_tokens_total: u64,
    /// Number of times `try_auto_compact` invoked `compact_conversation`.
    /// (Total number of times it fired past the threshold; counted as 1
    /// whether it ends in an internal skip, a summariser success, or any
    /// of the various `Err` outcomes.) Manual `attini session compact`
    /// does not hold `Counters`, so it is not recorded here.
    pub compaction_attempts: u64,
    /// Number of times `compact_conversation` returned `Err`.
    /// (Aggregates `Err` arising from `load_records_since_last_summary`,
    /// `run_summariser`, or the `?` in `session.append`.)
    pub compaction_failures: u64,
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
    pub skill_load: u64,
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
                "tool_calls.skill_load".to_string(),
                self.tool_calls_by_kind.skill_load,
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
            ("compaction_attempts".to_string(), self.compaction_attempts),
            ("compaction_failures".to_string(), self.compaction_failures),
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

/// Maximum prose characters sent to the summariser in a single
/// compaction pass. Roughly tokens ≈ chars/4 for ASCII-heavy tool
/// output, so 200 000 chars ≈ 50 000 tokens — inside even a 64 K
/// context with room for the ~500-word response. When the rendered
/// transcript exceeds this the newest portion is kept and the
/// oldest records are dropped.
pub const SUMMARY_MAX_CHARS: usize = 200_000;

/// Maximum prose characters kept from a single assistant/user record
/// before it is truncated in a summary transcript.
pub const SUMMARY_RECORD_MAX_CHARS: usize = 16_000;

/// Maximum prose characters kept from a single tool result in a
/// summary transcript.
pub const SUMMARY_TOOL_RESULT_MAX_CHARS: usize = 200;

/// Maximum raw character size of the retained record tail after a
/// compaction pass. If the newest records themselves are enormous
/// (a giant tool result), the cutoff walks further back so they are
/// folded into the summary rather than left to blow up the main
/// model call. 250 000 chars ≈ 62 000 tokens, inside even a 64 K
/// context; normal recent tails are far smaller and unaffected.
pub const RETAINED_TAIL_MAX_CHARS: usize = 250_000;

pub struct AgentConfig {
    pub session_name: String,
    pub model: String,
    /// Maximum completion tokens per model call. `None` uses the
    /// model's own default; `Some(n)` caps response size / cost.
    pub max_tokens: Option<u64>,
    pub workspace_root: PathBuf,
    pub system_prompt: Option<String>,
    pub show_reasoning: bool,
    pub max_turns: usize,
    pub mode: Mode,
    /// Extra workspace-external read-only path prefixes granted via
    /// `attini agent --read-path`. Combined with the persistent
    /// entries from `permissions.json.extra_read_paths` on startup.
    pub extra_read_paths_cli: Vec<PathBuf>,
    /// CLI `--reference` file paths to inject into the system prompt.
    /// Relative paths resolve against `workspace_root`. Files larger
    /// than [`REFERENCE_MAX_BYTES`] are not inlined; they are granted
    /// as extra read roots and referenced by absolute path instead.
    pub reference_paths: Vec<PathBuf>,
    /// Maximum tool calls admitted in a single model turn. Extras in
    /// the same response get a synthetic error result and the loop
    /// advances to the next turn.
    pub turn_tool_call_limit: usize,
    /// Sliding-window rate cap on admitted tool calls. `None`
    /// disables the check.
    pub tool_call_rate: Option<RateLimit>,
    /// Invocation-scope backstop on admitted tool calls. Hitting it
    /// stops the loop with [`InvocationEndReason::SessionToolCallExhausted`].
    /// `None` disables the check.
    pub session_tool_call_max: Option<usize>,
    /// Optional CLI-selected skill. When set, the resolved SKILL.md
    /// body is prepended as a system message before the first turn.
    /// Applies only to fresh invocations; combining with `--approve`
    /// / `--reject` is rejected in `main.rs`.
    pub skill_name: Option<String>,
    /// How side-effecting tool calls are authorized in this
    /// invocation. `plan run` supplies
    /// [`Authorization::ApprovedPlan`]; every other entry point uses
    /// the default `PerTool`.
    pub authorization: Authorization,
}

pub const DEFAULT_TURN_TOOL_CALL_LIMIT: usize = 20;
pub const DEFAULT_TOOL_CALL_RATE_CALLS: usize = 60;
pub const DEFAULT_TOOL_CALL_RATE_WINDOW_SECS: u64 = 60;
pub const DEFAULT_SESSION_TOOL_CALL_MAX: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub calls: usize,
    pub window: Duration,
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

/// Terminal outcome of one `agent_cli::run` invocation.
#[derive(Debug)]
pub enum RunOutcome {
    /// Normal exit with the process exit code.
    Exit(ExitCode),
}

pub fn run(cfg: AgentConfig, cont: Continuation) -> io::Result<RunOutcome> {
    let mut session = Session::open(&cfg.session_name)?;
    // Combine persistent extra_read_paths (from permissions.json) with
    // CLI --read-path overrides for this invocation, canonicalise
    // each, and hand the resulting Vec to the ToolExecutor. Any path
    // that fails to canonicalise is warned + skipped so a single bad
    // entry does not disable the whole grant list.
    let loaded = permissions::load(&cfg.session_name)?;
    let references = resolve_references(&cfg.workspace_root, &cfg.reference_paths)?;
    let mut candidates: Vec<PathBuf> = loaded.extra_read_paths.iter().map(PathBuf::from).collect();
    candidates.extend(cfg.extra_read_paths_cli.iter().cloned());
    candidates.extend(
        references
            .iter()
            .filter(|r| r.inline.is_none())
            .map(|r| r.abs_path.clone()),
    );
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

    // One-line start-of-invocation breadcrumb to stderr (a diagnostic, not
    // machine-consumed output). Printed BEFORE the model runs: it tells the
    // human which session/model is about to advance and how big the current
    // conversation already is (the last recorded `prompt_tokens`). `ctx=`
    // comes from the last recorded `prompt_tokens` (the conversation size
    // so far), not the cumulative billed total. Disabled with
    // ATTINI_STATUS_LINE=0.
    if std::env::var("ATTINI_STATUS_LINE").as_deref() != Ok("0") {
        let ctx_tokens = session.latest_prompt_tokens().ok().flatten().unwrap_or(0);
        eprintln!(
            "{}",
            render_agent_status_line(&cfg.model, &cfg.session_name, ctx_tokens)
        );
    }

    let mut counters = Counters::default();
    let outcome = drive(&mut session, &executor, &cfg, cont, &mut counters);

    let (reason, exit_code) = match &outcome {
        Ok(Driven::Completed) => (InvocationEndReason::Completed, EXIT_OK),
        Ok(Driven::AwaitingApproval) => (
            InvocationEndReason::AwaitingApproval,
            EXIT_AWAITING_APPROVAL,
        ),
        Ok(Driven::SessionToolCallExhausted) => {
            (InvocationEndReason::SessionToolCallExhausted, EXIT_ERROR)
        }
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
        Ok(_) => Ok(RunOutcome::Exit(ExitCode::from(exit_code))),
        Err(e) => {
            eprintln!("attini: {e}");
            Ok(RunOutcome::Exit(ExitCode::from(EXIT_ERROR)))
        }
    }
}

/// Render the single-line start-of-invocation breadcrumb written to stderr
/// by [`run`]. Pure function so the format can be unit-tested without
/// touching stdout. `ctx=` is the current conversation size (the last
/// recorded `prompt_tokens`) handed in by the caller; it is not the
/// cumulative billed total.
fn render_agent_status_line(model: &str, session_name: &str, ctx_tokens: u64) -> String {
    format!(
        "[agent] model={} session={} ctx={}",
        model, session_name, ctx_tokens,
    )
}

enum Driven {
    Completed,
    AwaitingApproval,
    /// Invocation-scope tool-call backstop tripped
    /// ([`AgentConfig::session_tool_call_max`]).
    SessionToolCallExhausted,
}

/// Enforces the three tool-call caps (per-turn, sliding rate window,
/// invocation-scope backstop) in `drive`'s tool_calls dispatch loop.
/// Counters increment only on admitted calls: rate-window and
/// session-cumulative do not consume budget when a cap already
/// rejected the call.
struct ToolCallGate {
    turn_limit: usize,
    rate: Option<RateLimit>,
    session_max: Option<usize>,
    turn_count: usize,
    rate_deque: VecDeque<Instant>,
    session_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum GateDecision {
    Proceed,
    TurnLimitExceeded,
    RateLimitExceeded,
    SessionExhausted,
}

impl ToolCallGate {
    fn new(cfg: &AgentConfig) -> Self {
        Self {
            turn_limit: cfg.turn_tool_call_limit,
            rate: cfg.tool_call_rate,
            session_max: cfg.session_tool_call_max,
            turn_count: 0,
            rate_deque: VecDeque::new(),
            session_count: 0,
        }
    }

    fn begin_turn(&mut self) {
        self.turn_count = 0;
    }

    /// Check whether one more tool call may proceed. On `Proceed`,
    /// admit the call and record it (turn counter, session counter,
    /// and rate window). On any rejection, do not consume budget.
    fn admit(&mut self, now: Instant) -> GateDecision {
        if self.turn_count >= self.turn_limit {
            return GateDecision::TurnLimitExceeded;
        }
        if let Some(rate) = self.rate {
            let cutoff = now.checked_sub(rate.window).unwrap_or(now);
            while self.rate_deque.front().is_some_and(|t| *t < cutoff) {
                self.rate_deque.pop_front();
            }
            if self.rate_deque.len() >= rate.calls {
                return GateDecision::RateLimitExceeded;
            }
        }
        if let Some(max) = self.session_max
            && self.session_count >= max
        {
            return GateDecision::SessionExhausted;
        }
        self.turn_count += 1;
        self.session_count += 1;
        if self.rate.is_some() {
            self.rate_deque.push_back(now);
        }
        GateDecision::Proceed
    }
}

fn drive(
    session: &mut Session,
    executor: &ToolExecutor,
    cfg: &AgentConfig,
    cont: Continuation,
    counters: &mut Counters,
) -> io::Result<Driven> {
    if matches!(cont, Continuation::Prompt(_)) {
        try_auto_compact(session, &cfg.model, counters, cfg.max_tokens)?;
    }

    let is_prompt = matches!(cont, Continuation::Prompt(_));
    let is_approve_or_reject = matches!(cont, Continuation::Approve | Continuation::Reject);

    let mut messages = build_initial_messages(session, cfg)?;

    // The `Prompt` path appends a fresh user record before the model
    // call. Any assistant `tool_call` left unanswered when the loop
    // previously suspended must be answered *before* that user record
    // is appended, otherwise a synthetic tool result would be
    // persisted after the user and break the assistant -> tool
    // continuity the API requires.
    if is_prompt {
        let repaired = repair_orphaned_tool_calls(session, &mut messages)?;
        if repaired > 0 {
            eprintln!(
                "[repair] inserted {repaired} synthetic tool result(s) for unanswered tool_call(s)"
            );
        }
    }

    match cont {
        Continuation::Prompt(text) => {
            session.append(&SessionRecord::User {
                ts: now_unix_millis(),
                text: text.clone(),
            })?;
            messages.push(ChatMessage::User(text));
        }
        Continuation::Approve => {
            let pendings = load_pending_or_err(session)?;
            for pending in &pendings {
                session.append(&SessionRecord::ToolApproval {
                    ts: now_unix_millis(),
                    call_id: pending.call_id.clone(),
                    decision: ApprovalDecision::Approve,
                    auto_decided_by: None,
                })?;
                let content = execute_pending(pending, executor)?;
                append_tool(session, &mut messages, &pending.call_id, content)?;
            }
            session.clear_pending()?;
        }
        Continuation::Reject => {
            let pendings = load_pending_or_err(session)?;
            for pending in &pendings {
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
            }
            session.clear_pending()?;
        }
    }

    // `Approve` / `Reject` consume the parked pending inside the match
    // above (appending a Tool record for the answered call), so they
    // run the orphan repair *after* the match; otherwise the pending
    // is still parked and the freshly-appended Tool record could be
    // re-surfaced as an orphan.
    if is_approve_or_reject {
        let repaired = repair_orphaned_tool_calls(session, &mut messages)?;
        if repaired > 0 {
            eprintln!(
                "[repair] inserted {repaired} synthetic tool result(s) for unanswered tool_call(s)"
            );
        }
    }

    let tools = build_tool_defs();
    let rules = permissions::load(&cfg.session_name)?;
    let mut gate = ToolCallGate::new(cfg);

    for _ in 0..cfg.max_turns {
        gate.begin_turn();
        let request = ChatRequest::new(cfg.model.clone(), messages.clone())
            .with_tools(tools.clone())
            .with_max_tokens(cfg.max_tokens);
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
            counters.prompt_tokens_last = usage.prompt_tokens.unwrap_or(0);
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

        // Batch-approval state: every tool call in this turn that needs
        // human approval is parked here. Once the first one is seen we
        // stop executing side-effecting siblings (auto-approved patches
        // / commands) so the assistant `tool_calls` list and the answer
        // `tool` messages stay in the same order; the pending ones are
        // parked and the rest are left for the orphan-repair pass to
        // cancel, after which the model re-issues them.
        let mut parked: Vec<Pending> = Vec::new();
        let mut suspending = false;

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
                "skill_load" => counters.tool_calls_by_kind.skill_load += 1,
                _ => counters.tool_calls_by_kind.unknown += 1,
            }
            match gate.admit(Instant::now()) {
                GateDecision::Proceed => {}
                GateDecision::TurnLimitExceeded => {
                    let content = tool_error_json(
                        "turn_tool_call_limit_exceeded",
                        &format!(
                            "turn_tool_call_limit={} exceeded in this turn",
                            cfg.turn_tool_call_limit
                        ),
                    );
                    eprintln!(
                        "[cap] turn_tool_call_limit={} exceeded",
                        cfg.turn_tool_call_limit
                    );
                    counters.tool_errors += 1;
                    append_tool(session, &mut messages, &tc.id, content)?;
                    continue;
                }
                GateDecision::RateLimitExceeded => {
                    let rate = cfg
                        .tool_call_rate
                        .expect("rate cap must be Some to hit RateLimitExceeded");
                    let content = tool_error_json(
                        "tool_call_rate_exceeded",
                        &format!(
                            "tool_call_rate={}/{}s exceeded",
                            rate.calls,
                            rate.window.as_secs()
                        ),
                    );
                    eprintln!(
                        "[cap] tool_call_rate={}/{}s exceeded",
                        rate.calls,
                        rate.window.as_secs()
                    );
                    counters.tool_errors += 1;
                    append_tool(session, &mut messages, &tc.id, content)?;
                    continue;
                }
                GateDecision::SessionExhausted => {
                    let max = cfg
                        .session_tool_call_max
                        .expect("session cap must be Some to hit SessionExhausted");
                    eprintln!("[cap] session_tool_call_max={max} exhausted; ending invocation");
                    return Ok(Driven::SessionToolCallExhausted);
                }
            }
            match classify(&tc.function_name) {
                ToolKind::ReadOnly => {
                    if suspending {
                        // Left unanswered so the orphan-repair pass can
                        // cancel it in tool-call order on resume.
                    } else {
                        let (summary, content, errored) = run_read_only(tc, executor);
                        eprintln!("{summary}");
                        if errored {
                            counters.tool_errors += 1;
                        }
                        append_tool(session, &mut messages, &tc.id, content)?;
                    }
                }
                ToolKind::Patch => {
                    if let PatchDispatch::Awaiting(pending) = dispatch_patch_unapproved(
                        tc,
                        executor,
                        session,
                        &mut messages,
                        counters,
                        suspending,
                    )? {
                        parked.push(pending);
                        suspending = true;
                    }
                }
                ToolKind::Command => {
                    if let CommandDispatch::Awaiting(pending) = dispatch_command(
                        tc,
                        executor,
                        cfg.mode,
                        &rules,
                        &cfg.authorization,
                        session,
                        &mut messages,
                        counters,
                        suspending,
                    )? {
                        parked.push(pending);
                        suspending = true;
                    }
                }
                ToolKind::Skill => {
                    if suspending {
                        // Left unanswered; cancelled on resume.
                    } else {
                        let content = run_skill_load(tc, counters);
                        append_tool(session, &mut messages, &tc.id, content)?;
                    }
                }
                ToolKind::Unknown => {
                    if suspending {
                        // Left unanswered; cancelled on resume.
                    } else {
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

        if !parked.is_empty() {
            session.save_pending(&parked)?;
            return Ok(Driven::AwaitingApproval);
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

/// Files larger than this are not inlined into the system prompt by
/// `attini agent --reference`; they are granted as extra read roots
/// and referenced by absolute path instead.
pub const REFERENCE_MAX_BYTES: usize = 32 * 1024;

/// A resolved `--reference` file. Small files carry their content in
/// `inline`; oversized files have `None` and are granted as extra
/// read roots and referenced by absolute path instead.
#[derive(Debug)]
struct ResolvedReference {
    abs_path: PathBuf,
    inline: Option<String>,
}

/// Resolve `--reference` paths against `workspace_root`, read each
/// file, and decide whether it can be inlined. Read failures are
/// surfaced as a startup error so the user sees the reason immediately.
fn resolve_references(
    workspace_root: &std::path::Path,
    reference_paths: &[PathBuf],
) -> io::Result<Vec<ResolvedReference>> {
    let mut out = Vec::with_capacity(reference_paths.len());
    for p in reference_paths {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            workspace_root.join(p)
        };
        let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
        let bytes = std::fs::read(&abs)
            .map_err(|e| io::Error::other(format!("--reference {}: {e}", abs.display())))?;
        let inline = if bytes.len() <= REFERENCE_MAX_BYTES {
            Some(String::from_utf8(bytes).map_err(|e| {
                io::Error::other(format!(
                    "--reference {}: not valid UTF-8: {e}",
                    abs.display()
                ))
            })?)
        } else {
            None
        };
        out.push(ResolvedReference {
            abs_path: abs,
            inline,
        });
    }
    Ok(out)
}

fn build_initial_messages(session: &Session, cfg: &AgentConfig) -> io::Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    messages.push(ChatMessage::System(render_workspace_context(
        &cfg.workspace_root,
    )));
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
    let discovered = skills::discover_all();
    if !discovered.is_empty() {
        messages.push(ChatMessage::System(render_available_skills(&discovered)));
    }
    if let Some(sys) = &cfg.system_prompt {
        messages.push(ChatMessage::System(sys.clone()));
    }
    if let Some(name) = &cfg.skill_name {
        let body = load_cli_skill_body(name)?;
        messages.push(ChatMessage::System(body));
    }
    let references = resolve_references(&cfg.workspace_root, &cfg.reference_paths)?;
    if !references.is_empty() {
        let mut ref_block = String::from("# Reference files\n\n");
        for r in &references {
            match &r.inline {
                Some(content) => {
                    ref_block.push_str(&format!("### {}\n\n{}\n\n", r.abs_path.display(), content));
                }
                None => {
                    ref_block.push_str(&format!(
                        "- {} — too large to inline; readable via the `read` tool at this absolute path\n",
                        r.abs_path.display()
                    ));
                }
            }
        }
        messages.push(ChatMessage::System(ref_block));
    }
    messages.push(ChatMessage::System(render_scratchpad_note(
        &cfg.session_name,
    )));
    for record in session.load_records_since_last_summary()? {
        messages.push(record.message);
    }
    Ok(messages)
}

/// Format the discovery listing that goes into the system prompt so
/// the model knows which skills are available. Missing descriptions
/// fall back to `(no description)` silently; oversize / broken
/// skills carry an explanatory marker so the user can spot them but
/// the listing is not otherwise noisy.
fn render_available_skills(entries: &[SkillEntry]) -> String {
    let mut out = String::from("# Available skills\n\n");
    for entry in entries {
        let label = if entry.oversized {
            "(too large; skill_load will fail)".to_string()
        } else if entry.broken {
            "(cannot read SKILL.md)".to_string()
        } else {
            entry
                .description
                .clone()
                .unwrap_or_else(|| "(no description)".to_string())
        };
        out.push_str(&format!("- {} — {}\n", entry.name, label));
    }
    out
}

/// Tell the model it may keep working notes under the session's
/// scratchpad directory. The patch tool permits writes there (it is
/// not rejected by the Layer-1 `.attini/` guard), but because the
/// files are not git-tracked those edits still go through the
/// approval prompt, so the note is honest about that rather than
/// promising an auto-approve free zone.
fn render_scratchpad_note(session_name: &str) -> String {
    format!(
        "# Working notes\n\n\
         You may keep working notes / scratchpad files under \
         `.attini/{session_name}/scratchpad/` (relative to the workspace root). \
         This per-session directory is not tracked by git and never appears in \
         `git diff`. Use it for checklists, intermediate findings, or step lists \
         that would otherwise clutter the conversation. Because files there are \
         not tracked, `patch` writes are permitted but are shown for approval, \
         like any other non-tracked write.\n"
    )
}

/// Render a best-effort `# Workspace context` block so the model can
/// see where the session is rooted: the absolute current directory
/// (the workspace root), the enclosing git repository (when present),
/// the current branch, and
/// whether the root is a linked git worktree. Branch / repo awareness
/// is the main value — without it the model edits files without
/// knowing which repo or branch it is on.
///
/// Never noisy or fatal: any git failure degrades to just the
/// workspace path line, and stderr is swallowed so a missing / invalid
/// repo does not clutter the prompt or duplicate the warning that
/// `probe_git_state` emits.
fn render_workspace_context(root: &Path) -> String {
    let mut out = String::from("# Workspace context\n\n");
    out.push_str(&format!(
        "- Current directory: {} (workspace root)\n",
        root.display()
    ));

    // Best-effort git probe. Call via `git -C <root>` so the current
    // working dir is irrelevant and a linked worktree resolves to its
    // own toplevel / index.
    fn git_quiet(root: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    let toplevel = git_quiet(root, &["rev-parse", "--show-toplevel"]);
    let branch = git_quiet(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let git_dir = git_quiet(root, &["rev-parse", "--absolute-git-dir"]);

    match &toplevel {
        Some(top) => {
            out.push_str(&format!("- Git repository: {}\n", top));
            if let Some(br) = &branch {
                out.push_str(&format!("- Current branch: {}\n", br));
            }
            // A linked worktree reports a git dir under
            // `<repo>/.git/worktrees/<name>`; a normal repo's git dir is
            // the `.git` dir itself. Only surface the *fact* that the
            // root is a linked worktree — the raw git-dir path is an
            // implementation detail that adds nothing for the model.
            if let Some(gd) = &git_dir
                && (gd.contains("/worktrees/") || gd.contains("\\worktrees\\"))
            {
                out.push_str("- Linked worktree\n");
            }
        }
        None => {
            out.push_str("- Not inside a git repository\n");
        }
    }
    out
}

/// Resolve and load a CLI-selected skill. Called at the start of a
/// fresh `attini agent --skill NAME` invocation. Any failure
/// (missing / too large / bad UTF-8) is surfaced as a startup
/// `io::Error` so the user sees the reason immediately, rather than
/// the model getting a mysterious empty system message.
fn load_cli_skill_body(name: &str) -> io::Result<String> {
    let Some(dir) = skills::resolve_skill_dir(name) else {
        return Err(io::Error::other(format!(
            "--skill {name}: no SKILL.md found under any skill root"
        )));
    };
    let skill_md = dir.join("SKILL.md");
    skills::load_body(&skill_md).map_err(|e| {
        let (_, msg) = e.to_code_and_message();
        io::Error::other(format!("--skill {name}: {msg}"))
    })
}

/// Dispatch a `skill_load` tool call: parse, resolve, load, return
/// the tool_result content (either the skill body verbatim or a
/// `tool_error_json`). Increments `tool_errors` on failure.
fn run_skill_load(tc: &ToolCall, counters: &mut Counters) -> String {
    let inv = match SkillLoadInvocation::parse(&tc.arguments_json) {
        Ok(inv) => inv,
        Err(err) => {
            counters.tool_errors += 1;
            eprintln!("[skill_load] parse err: {err:?}");
            return tool_error_json_from(&err);
        }
    };
    let Some(dir) = skills::resolve_skill_dir(&inv.name) else {
        counters.tool_errors += 1;
        eprintln!("[skill_load] not found: {}", inv.name);
        return tool_error_json(
            "skill_not_found",
            &format!("no SKILL.md found for skill {:?}", inv.name),
        );
    };
    let skill_md = dir.join("SKILL.md");
    match skills::load_body(&skill_md) {
        Ok(body) => {
            eprintln!("[skill_load] {}", inv.name);
            body
        }
        Err(e) => {
            counters.tool_errors += 1;
            let (code, message) = e.to_code_and_message();
            eprintln!("[skill_load] {}: {message}", inv.name);
            tool_error_json(code, &message)
        }
    }
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

fn try_auto_compact(
    session: &mut Session,
    model: &str,
    counters: &mut Counters,
    max_tokens: Option<u64>,
) -> io::Result<()> {
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
    counters.compaction_attempts += 1;
    if let Err(e) = compact_conversation(session, model, max_tokens) {
        counters.compaction_failures += 1;
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
pub fn compact_conversation(
    session: &mut Session,
    model: &str,
    max_tokens: Option<u64>,
) -> io::Result<()> {
    let records = session.load_records_since_last_summary()?;
    let Some(keep_start) = compaction_cutoff(
        &records,
        KEEP_RECENT_RECORDS_TARGET,
        RETAINED_TAIL_MAX_CHARS,
    ) else {
        eprintln!("[compaction] no records eligible for summarisation. skipping.");
        return Ok(());
    };
    let to_summarise: Vec<ChatMessageWithTs> = if keep_start == records.len() {
        // Folding every record; the retained tail is empty so the
        // summary alone becomes the history for the next call.
        records.clone()
    } else {
        records[..keep_start].to_vec()
    };
    let record_count = to_summarise.len();
    let since_ts = to_summarise
        .first()
        .map(|r| r.ts)
        .expect("to_summarise is non-empty");
    let cutoff_ts = to_summarise
        .last()
        .map(|r| r.ts)
        .expect("to_summarise is non-empty");

    let text = run_summariser(model, to_summarise, max_tokens)?;
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

const ASK_SYSTEM_PROMPT: &str = "You are an OUTSIDE observer reading a recording of a \
coding-agent session. You are NOT the coding assistant and you are NOT continuing \
its work: do not emit tool calls, do not write plan steps, do not pick up an \
unfinished action, do not reproduce the session's agentic wording. Read the session \
transcript below (rendered as plain prose; tool calls and results are abbreviated) \
and answer directly and concisely about what is happening now: unfinished work, \
decisions, files/symbols in play, and any tool call awaiting approval. If a question \
is appended, answer that question specifically. Otherwise produce a short status \
summary (~300 words) of the current state, in third person. Never begin with an \
action verb such as 'I will / I am going to / let's'. Do not comment on the \
instruction itself; produce only the answer. A prior observer answer may be included \
    below as context: treat it as a hint only and always let the transcript below \
    override it.";

/// Abbreviate JSON tool arguments to a short single-line prefix so the
/// prose transcript stays readable and does not invite the model to
/// reproduce the raw agentic tool-call format.
fn abbreviate_args(args_json: &str) -> String {
    let t = args_json.trim();
    if t.is_empty() {
        return String::new();
    }
    let mut cleaned = String::new();
    for c in t.chars().take(90) {
        cleaned.push(if c == '\n' { ' ' } else { c });
    }
    if t.chars().count() > 90 {
        cleaned.push('…');
    }
    cleaned
}

/// Render conversation records as a plain third-person prose transcript
/// with tool calls and results abbreviated. This deliberately strips the
/// raw `tool_calls`/`tool` JSON and any `<invoke>`-style XML so a
/// summariser model does not imitate the coding-agent's tool-calling
/// format and instead behaves as an outside observer.
fn render_records_prose(records: &[ChatMessageWithTs]) -> String {
    let mut out = String::new();
    for rec in records {
        match &rec.message {
            ChatMessage::System(s) => out.push_str(&format!("[system] {}\n", s.trim())),
            ChatMessage::User(s) => out.push_str(&format!("user: {}\n", s.trim())),
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let c = content.trim();
                let mut lines = Vec::new();
                if !c.is_empty() {
                    lines.push(c.to_string());
                }
                for tc in tool_calls {
                    let a = abbreviate_args(&tc.arguments_json);
                    if a.is_empty() {
                        lines.push(format!("  [tool call: {}]", tc.function_name));
                    } else {
                        lines.push(format!("  [tool call: {} ({})]", tc.function_name, a));
                    }
                }
                if !lines.is_empty() {
                    out.push_str(&format!("assistant: {}\n", lines.join("\n")));
                }
            }
            ChatMessage::Tool { content, .. } => {
                let t = content.trim();
                let brief = if t.is_empty() {
                    String::new()
                } else {
                    let mut s = String::new();
                    for c in t.chars().take(200) {
                        s.push(c);
                    }
                    if t.chars().count() > 200 {
                        s.push('…');
                    }
                    s
                };
                out.push_str(&format!("  [tool result: {}]\n", brief));
            }
        }
        out.push('\n');
    }
    out
}

/// Truncate a prose segment to `max_chars` characters, keeping the
/// head and appending a marker that notes how many were dropped.
fn truncate_prose(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…[truncated {} chars]", count - max_chars)
}

/// Render a single record into a bounded prose block for the
/// summariser. Assistant/user content is capped, tool-call arguments
/// are abbreviated, and tool results are truncated to a short line, so
/// an enormous conversation (huge tool results) never blows up the
/// summariser request.
fn render_summary_block(rec: &ChatMessageWithTs) -> String {
    match &rec.message {
        ChatMessage::System(s) => format!(
            "[system] {}\n",
            truncate_prose(s.trim(), SUMMARY_RECORD_MAX_CHARS)
        ),
        ChatMessage::User(s) => format!(
            "user: {}\n",
            truncate_prose(s.trim(), SUMMARY_RECORD_MAX_CHARS)
        ),
        ChatMessage::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let mut lines = Vec::new();
            let c = content.trim();
            if !c.is_empty() {
                lines.push(truncate_prose(c, SUMMARY_RECORD_MAX_CHARS));
            }
            for tc in tool_calls {
                let a = abbreviate_args(&tc.arguments_json);
                if a.is_empty() {
                    lines.push(format!("  [tool call: {}]", tc.function_name));
                } else {
                    lines.push(format!("  [tool call: {} ({})]", tc.function_name, a));
                }
            }
            if lines.is_empty() {
                String::new()
            } else {
                format!("assistant: {}\n", lines.join("\n"))
            }
        }
        ChatMessage::Tool { content, .. } => {
            let t = content.trim();
            let brief = truncate_prose(t, SUMMARY_TOOL_RESULT_MAX_CHARS);
            format!("  [tool result: {}]\n", brief)
        }
    }
}

/// Render conversation records into a bounded prose transcript for the
/// summariser. Each record is rendered into a small bounded block, and
/// the blocks are kept newest-first until the total reaches
/// [`SUMMARY_MAX_CHARS`]; older blocks are dropped. When anything is
/// dropped a note is prepended so the resulting summary reflects the
/// most recent state.
fn render_summary_transcript(records: &[ChatMessageWithTs]) -> String {
    let blocks: Vec<String> = records.iter().map(render_summary_block).collect();
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut dropped_oldest = false;
    for block in blocks.iter().rev() {
        let len = block.chars().count();
        if total + len > SUMMARY_MAX_CHARS {
            dropped_oldest = true;
            break;
        }
        kept.push(block.clone());
        total += len;
    }
    let mut out = String::new();
    if dropped_oldest {
        out.push_str(
            "[Note: the earliest records of this segment were dropped to fit the \
             summariser's context window; the transcript below is the most recent \
             portion, so the summary should reflect the current state.]\n\n",
        );
    }
    for block in kept.into_iter().rev() {
        out.push_str(&block);
        out.push('\n');
    }
    out
}

fn call_summariser_messages(
    model: &str,
    messages: Vec<ChatMessage>,
    max_tokens: Option<u64>,
) -> io::Result<String> {
    let request = ChatRequest::new(model.to_string(), messages).with_max_tokens(max_tokens);
    let mut sink = io::sink();
    let mut sinks = ProgressSinks {
        content: &mut sink,
        reasoning: None,
    };
    let result = curl::call(&request, &mut sinks)
        .map_err(|e| io::Error::other(format!("summariser call failed: {e}")))?;
    pick_summary_text(&result).ok_or_else(|| {
        io::Error::other(
            "summariser returned empty content \
             (both content and reasoning_content were empty)",
        )
    })
}

fn run_summariser(
    model: &str,
    records: Vec<ChatMessageWithTs>,
    max_tokens: Option<u64>,
) -> io::Result<String> {
    // Render the records as a bounded prose transcript rather than
    // sending the raw ChatMessages. Raw messages include the full
    // tool-result JSON, which can be enormous and push the request
    // past the model context window so compaction fails and the
    // invocation later fails too. The prose form preserves the
    // semantic thread (assistant conclusions, decisions, file/symbol
    // mentions) while keeping each tool result to a short line.
    let transcript = render_summary_transcript(&records);
    let messages = vec![
        ChatMessage::System(SUMMARIZER_SYSTEM_PROMPT.to_string()),
        ChatMessage::User(transcript),
    ];
    call_summariser_messages(model, messages, max_tokens)
}

/// Read-only model summarisation used by `attini ask`. Unlike
/// `run_summariser` this never persists anything; it just answers a
/// (optional) question about the current session state.
pub(crate) fn run_ask_summary(
    records: Vec<ChatMessageWithTs>,
    model: &str,
    question: Option<&str>,
    prior: Option<&str>,
    max_tokens: Option<u64>,
) -> io::Result<String> {
    let mut system = ASK_SYSTEM_PROMPT.to_string();
    if let Some(p) = prior {
        system.push_str(
            "\n\n--- PREVIOUS ask context (an EARLIER observer answer; it is a HINT, not \
             ground truth \u{2014} the transcript below is authoritative) ---\n\n",
        );
        system.push_str(p);
        system.push_str("\n\n--- END PREVIOUS ask context ---\n");
    }
    if let Some(q) = question {
        system.push_str("\n\nThe user's question is: ");
        system.push_str(q);
        system.push('\n');
    }
    system.push_str("\n\n--- BEGIN SESSION TRANSCRIPT (prose) ---\n\n");
    system.push_str(&render_records_prose(&records));
    system.push_str("\n--- END SESSION TRANSCRIPT ---\n");
    call_summariser_messages(model, vec![ChatMessage::System(system)], max_tokens)
}

/// Pick a usable summary from a [`CallResult`]: prefer `content` and
/// fall back to `reasoning_content` when the primary field is empty.
/// Some reasoning-capable DeepSeek models emit the actual answer via
/// `reasoning_content` while leaving `content` blank; without this
/// fallback auto-compaction fails on every such response and the
/// invocation carries the full history for the remaining turns.
fn pick_summary_text(result: &curl::CallResult) -> Option<String> {
    let content = result.content.trim();
    if !content.is_empty() {
        return Some(content.to_string());
    }
    result
        .reasoning_content
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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

/// Rough byte length of a message's payload. Used purely as an
/// order-of-magnitude heuristic for the retained-tail budget; an
/// ASCII-heavy tool result's bytes closely track its token count.
fn message_raw_char_len(msg: &ChatMessage) -> usize {
    match msg {
        ChatMessage::System(s) => s.len(),
        ChatMessage::User(s) => s.len(),
        ChatMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            content.len()
                + reasoning_content.as_deref().map_or(0, |r| r.len())
                + tool_calls
                    .iter()
                    .map(|tc| tc.function_name.len() + tc.arguments_json.len())
                    .sum::<usize>()
        }
        ChatMessage::Tool { content, .. } => content.len(),
    }
}

/// Choose the compaction cutoff, returning `None` when there is
/// nothing to compact (too few records) and `Some(keep_start)`
/// otherwise. `keep_start` is the index where the retained tail
/// begins; `Some(n)` (the record count) means fold every record into
/// the summary, leaving an empty retained tail.
///
/// Beyond the record-count target in [`safe_tail_start`], the cutoff
/// is walked **forward** while the retained tail's raw size exceeds
/// `max_tail_chars`. A retained tail that is oversized because of a
/// huge record at the very end (a session suspended right after a
/// giant tool result) cannot be shrunk by folding older records, so
/// the walk folds toward the end and finally folds everything.
fn compaction_cutoff(
    records: &[ChatMessageWithTs],
    target_keep: usize,
    max_tail_chars: usize,
) -> Option<usize> {
    let n = records.len();
    if n <= target_keep {
        return None;
    }
    let mut keep_start = safe_tail_start(records, target_keep);
    // If the retained tail cannot fit, fold its oldest part by moving
    // the cutoff toward the end, stopping at a safe boundary so an
    // `assistant -> tool` pair is never split.
    while keep_start < n {
        let tail_chars: usize = records[keep_start..]
            .iter()
            .map(|r| message_raw_char_len(&r.message))
            .sum();
        if tail_chars <= max_tail_chars {
            break;
        }
        let mut next = keep_start + 1;
        while next < n && !is_safe_boundary(&records[next].message) {
            next += 1;
        }
        // `next` may reach `n`, which folds everything.
        keep_start = next;
    }
    if keep_start == 0 {
        // The whole history fits the budget but no safe boundary exists
        // in the initial retained window: nothing to safely fold, so
        // skip compaction rather than summarise the entire conversation.
        return None;
    }
    Some(keep_start)
}

fn build_tool_defs() -> Vec<ToolDef> {
    let mut defs = ReadOnlyTool::definitions();
    defs.push(PatchInvocation::definition());
    defs.push(CommandInvocation::definition());
    defs.push(SkillLoadInvocation::definition());
    defs
}

enum CommandDispatch {
    /// The call needs human approval; carries the parked pending.
    Awaiting(Pending),
    /// The call was handled (or, in `dry_run`, can be skipped).
    Continue,
}

#[allow(clippy::too_many_arguments)]
fn dispatch_command(
    tc: &ToolCall,
    executor: &ToolExecutor,
    mode: Mode,
    rules: &LoadedRules,
    authorization: &Authorization,
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
    counters: &mut Counters,
    dry_run: bool,
) -> io::Result<CommandDispatch> {
    let inv = match CommandInvocation::parse(&tc.arguments_json) {
        Ok(inv) => inv,
        Err(err) => {
            if !dry_run {
                let content = tool_error_json("command_args", &format!("{err:?}"));
                eprintln!("[command] parse err: {err:?}");
                counters.tool_errors += 1;
                append_tool(session, messages, &tc.id, content)?;
            }
            return Ok(CommandDispatch::Continue);
        }
    };
    let judgment = evaluate(
        mode,
        &rules.session,
        &rules.workspace,
        &inv.argv,
        authorization,
    );
    let display = shell_escape_argv(&inv.argv);
    match judgment {
        Judgment::AutoApprove(dec) => {
            if !dry_run {
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
            }
            Ok(CommandDispatch::Continue)
        }
        Judgment::AutoDeny(dec) => {
            if !dry_run {
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
            }
            Ok(CommandDispatch::Continue)
        }
        Judgment::Pending => {
            let preview_text = render_command_preview_from(&inv);
            eprintln!("[command] approval required");
            eprintln!("{preview_text}");
            emit_suggested_rule(&inv.argv);
            Ok(CommandDispatch::Awaiting(build_pending(
                tc,
                PendingToolKind::Command,
                preview_text,
            )))
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
pub(crate) fn shell_single_quote(s: &str) -> String {
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
    Skill,
    Unknown,
}

fn classify(name: &str) -> ToolKind {
    match name {
        "list" | "read" | "search" => ToolKind::ReadOnly,
        "patch" => ToolKind::Patch,
        "command" => ToolKind::Command,
        "skill_load" => ToolKind::Skill,
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

enum PatchDispatch {
    /// The call needs human approval; carries the parked pending.
    Awaiting(Pending),
    /// The call was handled (or, in `dry_run`, can be skipped).
    Continue,
}

/// Dispatch a patch tool call outside an approved-plan run.
///
/// Edits on git-tracked files are auto-applied immediately (git makes
/// them revertible, so no human prompt is needed). Any other edit
/// (a new file, a scratchpad / non-tracked target) parks a pending
/// approval and returns [`PatchDispatch::Awaiting`] so the caller
/// suspends the invocation.
///
/// When `dry_run` is `true` (a later sibling already needs approval)
/// the working tree is never mutated: auto-approved edits are
/// skipped and preview errors are not appended. The caller still
/// receives `Awaiting(Pending)` for edits that need a human.
fn dispatch_patch_unapproved(
    tc: &ToolCall,
    executor: &ToolExecutor,
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
    counters: &mut Counters,
    dry_run: bool,
) -> io::Result<PatchDispatch> {
    let inv = match PatchInvocation::parse(&tc.arguments_json) {
        Ok(inv) => inv,
        Err(err) => {
            if !dry_run {
                let content = tool_error_json("patch_args", &format!("{err:?}"));
                eprintln!("[patch] parse err: {err:?}");
                counters.tool_errors += 1;
                append_tool(session, messages, &tc.id, content)?;
            }
            return Ok(PatchDispatch::Continue);
        }
    };
    let (hashes, preview) = match executor.preview_patch(&inv) {
        Ok(x) => x,
        Err(e) => {
            if !dry_run {
                let content = tool_error_json("patch_preview", &format!("{e:?}"));
                eprintln!("[patch] preview err: {e:?}");
                counters.tool_errors += 1;
                append_tool(session, messages, &tc.id, content)?;
            }
            return Ok(PatchDispatch::Continue);
        }
    };
    if preview.auto_approve {
        if !dry_run {
            match executor.apply_patch(&inv, &hashes) {
                Ok(paths) => {
                    eprintln!(
                        "[patch] auto-approved: {} file(s) (git-tracked)",
                        paths.len()
                    );
                    append_tool(session, messages, &tc.id, patch_result_json(&paths))?;
                }
                Err(e) => {
                    let content = tool_error_json("patch_apply", &format!("{e:?}"));
                    eprintln!("[patch] apply err: {e:?}");
                    counters.tool_errors += 1;
                    append_tool(session, messages, &tc.id, content)?;
                }
            }
        }
        Ok(PatchDispatch::Continue)
    } else {
        let preview_text = render_patch_preview_text(&preview);
        eprintln!("[patch] approval required");
        eprintln!("{preview_text}");
        Ok(PatchDispatch::Awaiting(build_pending(
            tc,
            PendingToolKind::Patch,
            preview_text,
        )))
    }
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

fn build_pending(tc: &ToolCall, kind: PendingToolKind, preview: String) -> Pending {
    Pending {
        ts: now_unix_millis(),
        call_id: tc.id.clone(),
        tool_kind: kind,
        function_name: tc.function_name.clone(),
        arguments_json: tc.arguments_json.clone(),
        preview,
    }
}

fn execute_pending(pending: &Pending, executor: &ToolExecutor) -> io::Result<String> {
    match pending.tool_kind {
        PendingToolKind::Patch => {
            // Convert any patch parse / preview / apply failure into a
            // tool-error result so a batch approval always answers every
            // parked call instead of aborting mid-way and leaving a
            // partially-executed pending set behind.
            let inv = match PatchInvocation::parse(&pending.arguments_json) {
                Ok(inv) => inv,
                Err(e) => return Ok(tool_error_json("patch_args", &format!("{e:?}"))),
            };
            let (hashes, _preview) = match executor.preview_patch(&inv) {
                Ok(x) => x,
                Err(e) => return Ok(tool_error_json("patch_preview", &format!("{e:?}"))),
            };
            match executor.apply_patch(&inv, &hashes) {
                Ok(paths) => Ok(patch_result_json(&paths)),
                Err(e) => Ok(tool_error_json("patch_apply", &format!("{e:?}"))),
            }
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
    let mut cmd = Command::new(&inv.argv[0]);
    cmd.args(&inv.argv[1..]).current_dir(executor.root());
    let output =
        crate::child_output::run_streamed(&mut cmd).map_err(|e| CommandError::SpawnFailed {
            message: e.to_string(),
        })?;
    let elapsed = started.elapsed();
    let termination_reason = if output.status.code().is_some() {
        "exited"
    } else {
        "signaled"
    };
    Ok(command_result_json(
        &output.stdout,
        &output.stderr,
        output.status.code(),
        termination_reason,
        elapsed,
    ))
}

/// A synthetic tool result that must be persisted to the session
/// transcript and inserted into the in-memory message list because
/// the original `tool_call` was never answered (e.g. an unapproved
/// sibling left behind when the loop suspended awaiting approval).
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanedToolCall {
    call_id: String,
    content: String,
}

/// Pure, testable core of [`repair_orphaned_tool_calls`]: scan a
/// message list and return a repaired copy in which every assistant
/// message's unanswered `tool_calls` get an explicit reject / cancel
/// `tool` result inserted immediately after the assistant's existing
/// tool results, preserving conversation order. Also returns the
/// list of synthetic results so the caller can persist them to the
/// session transcript.
///
/// Rather than assuming tool results are contiguous, this indexes every
/// tool message by the `call_id` it answers, so an assistant's tool
/// results are associated with it even when the on-disk transcript has
/// interleaved records (e.g. a `user` turn appended between an
/// assistant's `tool_calls` and a synthetic reject persisted for it).
/// Each assistant's answers are emitted immediately after it and stray
/// / duplicate tool messages are dropped, restoring the
/// "assistant(tool_calls) -> tool, tool, ..." invariant that the API
/// requires.
///
/// An assistant message whose `tool_calls` are all answered is left
/// untouched; an assistant with no `tool_calls` is copied verbatim.
fn repair_messages(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<OrphanedToolCall>) {
    let mut tools_by_call: BTreeMap<String, VecDeque<ChatMessage>> = BTreeMap::new();
    for msg in messages {
        if let ChatMessage::Tool { tool_call_id, .. } = msg {
            tools_by_call
                .entry(tool_call_id.clone())
                .or_default()
                .push_back(msg.clone());
        }
    }

    let mut repaired: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut orphans: Vec<OrphanedToolCall> = Vec::new();
    // Call ids for which we already emitted (or synthesised) a tool
    // result in `repaired`. A later tool message for one of these ids
    // is a misplaced / duplicate record and is dropped.
    let mut placed: BTreeSet<String> = BTreeSet::new();

    for msg in messages {
        match msg {
            ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                repaired.push(msg.clone());
                for tc in tool_calls {
                    if placed.contains(&tc.id) {
                        continue;
                    }
                    if let Some(queue) = tools_by_call.get_mut(&tc.id)
                        && let Some(tool_msg) = queue.pop_front()
                    {
                        repaired.push(tool_msg);
                        placed.insert(tc.id.clone());
                        continue;
                    }
                    let content = tool_error_json(
                        "unanswered_tool_call",
                        "this tool call was left unapproved and is cancelled before continuing",
                    );
                    orphans.push(OrphanedToolCall {
                        call_id: tc.id.clone(),
                        content: content.clone(),
                    });
                    repaired.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content,
                    });
                    placed.insert(tc.id.clone());
                }
            }
            ChatMessage::Tool { tool_call_id, .. } => {
                if placed.contains(tool_call_id) {
                    continue; // already emitted as this assistant's tool result
                }
                placed.insert(tool_call_id.clone());
                repaired.push(msg.clone());
            }
            _ => repaired.push(msg.clone()),
        }
    }
    (repaired, orphans)
}

/// Repair a transcript before it is sent to the model: for every
/// assistant `tool_call` that has no answering `tool` result in
/// `messages`, synthesize an explicit reject / cancel result, insert
/// it at the correct position, and persist the corresponding
/// `ToolApproval` / `Tool` records to `session` so the on-disk
/// transcript is healed and the same orphan does not recur on a
/// later invocation. If the cancelled call corresponds to a pending
/// approval that the caller bypassed with a fresh prompt, the pending
/// is cleared so a later `--approve` / `--reject` does not double-run
/// it.
///
/// Even when no new synthetic result is required, the repaired message
/// order is always adopted. `repair_messages` may have moved a tool
/// result that was persisted out of order (e.g. after a `user` turn)
/// back to immediately follow its assistant, and that in-memory
/// correction is the list actually sent to the model; the on-disk
/// records are left intact.
///
/// Returns the number of synthetic tool results inserted.
fn repair_orphaned_tool_calls(
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
) -> io::Result<usize> {
    let (repaired, orphans) = repair_messages(messages);
    if !orphans.is_empty() {
        for orphan in &orphans {
            // If the orphan is the very call that the previous invocation
            // parked in pending.json and the caller resumed with a fresh
            // prompt (bypassing --approve / --reject), drop the pending so
            // a later resume does not execute the now-cancelled call.
            if let Some(pendings) = session.load_pending()?
                && pendings.iter().any(|p| p.call_id == orphan.call_id)
            {
                session.clear_pending()?;
            }
            session.append(&SessionRecord::ToolApproval {
                ts: now_unix_millis(),
                call_id: orphan.call_id.clone(),
                decision: ApprovalDecision::Reject,
                auto_decided_by: Some(AutoDecidedBy {
                    scope: "repair".to_string(),
                    argv_prefix: Vec::new(),
                    reason: "unanswered_tool_call_repair".to_string(),
                }),
            })?;
            session.append(&SessionRecord::Tool {
                ts: now_unix_millis(),
                call_id: orphan.call_id.clone(),
                content: orphan.content.clone(),
            })?;
            eprintln!(
                "[repair] cancelled unanswered tool_call {} (left unapproved)",
                orphan.call_id
            );
        }
    }
    *messages = repaired;
    Ok(orphans.len())
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

fn load_pending_or_err(session: &Session) -> io::Result<Vec<Pending>> {
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
    fn workspace_context_degrades_when_not_a_repo() {
        // Deterministic: a non-existent path is never a git repo, so the
        // block must degrade gracefully while still reporting the root.
        let ctx = render_workspace_context(Path::new("/definitely/not/a/repo"));
        assert!(ctx.starts_with("# Workspace context\n\n"));
        assert!(ctx.contains("- Current directory: /definitely/not/a/repo"));
        assert!(ctx.contains("- Not inside a git repository"));
    }

    #[test]
    fn workspace_context_is_well_formed_in_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let ctx = render_workspace_context(&cwd);
        assert!(ctx.starts_with("# Workspace context\n\n"));
        assert!(ctx.contains(&format!("- Current directory: {}", cwd.display())));
        // Either it found a git repo (and names it) or it did not; both
        // are acceptable. The key requirement is it never panics and
        // always states git status.
        let has_status =
            ctx.contains("- Git repository: ") || ctx.contains("- Not inside a git repository");
        assert!(has_status, "context should state git status: {ctx}");
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

    fn big_user(ts: u64, len: usize) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::User(format!("u{ts}:{}", "x".repeat(len))),
            ts,
        }
    }

    #[test]
    fn truncate_prose_keeps_head_and_notes_dropped_chars() {
        let s = "abcdefghij".repeat(2000); // 20_000 chars
        let out = truncate_prose(&s, 1000);
        assert!(out.starts_with("abc"));
        assert!(out.contains("[truncated 19000 chars]"));
        assert!(out.chars().count() < 2000);
    }

    #[test]
    fn render_summary_transcript_fits_all_within_budget() {
        let records = vec![user(1), assistant_plain(2), tool(3)];
        let out = render_summary_transcript(&records);
        assert!(!out.contains("[Note:"), "unexpected note: {out}");
        assert!(out.contains("user: u1"));
        assert!(out.contains("assistant: a2"));
        assert!(out.contains("[tool result:"));
    }

    #[test]
    fn render_summary_transcript_drops_oldest_when_over_budget() {
        // Each record is over SUMMARY_RECORD_MAX_CHARS so each renders
        // to ~SUMMARY_RECORD_MAX_CHARS of prose + a truncation marker.
        // Fourteen such blocks exceed SUMMARY_MAX_CHARS, so the newest
        // ones are kept and the oldest are dropped (with a note).
        let mut records = Vec::new();
        for i in 1..=14 {
            records.push(big_user(i, SUMMARY_RECORD_MAX_CHARS + 100));
        }
        let out = render_summary_transcript(&records);
        assert!(out.contains("[Note:"), "expected a truncation note");
        assert!(out.contains("u14:"), "newest record should be kept");
        let kept_oldest_marker = records
            .len()
            .checked_sub(2)
            .map(|_| format!("u{}:", records.len().saturating_sub(2)))
            .unwrap_or_default();
        // The newest two are definitely kept; the absolute oldest is dropped.
        assert!(!out.contains("u1:"), "oldest record should be dropped");
        assert!(
            out.contains(&kept_oldest_marker),
            "a kept record ({kept_oldest_marker}) should be present"
        );
    }

    #[test]
    fn render_summary_block_truncates_huge_tool_result() {
        let huge = "y".repeat(50_000);
        let rec = ChatMessageWithTs {
            message: ChatMessage::Tool {
                tool_call_id: "call_1".to_string(),
                content: huge,
            },
            ts: 1,
        };
        let out = render_summary_block(&rec);
        assert!(
            out.contains("[truncated"),
            "tool result not truncated: {}",
            out.len()
        );
        assert!(out.chars().count() < 1_000);
    }

    fn big_tool(ts: u64, len: usize) -> ChatMessageWithTs {
        ChatMessageWithTs {
            message: ChatMessage::Tool {
                tool_call_id: format!("call_{ts}"),
                content: "z".repeat(len),
            },
            ts,
        }
    }

    #[test]
    fn compaction_cutoff_returns_none_when_too_short() {
        let records = vec![user(1), assistant_plain(2)];
        assert_eq!(compaction_cutoff(&records, 10, 1000), None);
    }

    #[test]
    fn compaction_cutoff_returns_safe_tail_start_within_budget() {
        let records = vec![
            user(1),
            assistant_plain(2),
            user(3),
            assistant_plain(4),
            user(5),
        ];
        let keep = safe_tail_start(&records, 2);
        assert!(keep > 0);
        assert_eq!(compaction_cutoff(&records, 2, 100_000), Some(keep));
    }

    #[test]
    fn compaction_cutoff_folds_huge_tail_to_the_end() {
        // A giant tool result sits at the very end, with no safe
        // boundary after it. The retained tail cannot be shrunk by
        // folding older records, so the cutoff walks to the end and
        // everything is folded into the summary.
        let records = vec![
            user(1),
            assistant_plain(2),
            assistant_with_tool_call(3),
            big_tool(4, 50_000),
        ];
        assert_eq!(compaction_cutoff(&records, 2, 100), Some(records.len()));
    }

    #[test]
    fn compaction_cutoff_folds_middle_huge_record_normally() {
        // A huge tool result before a clearly safe recent tail does not
        // force fold-all: safe_tail_start already lands after it.
        let records = vec![
            user(1),
            assistant_with_tool_call(2),
            big_tool(3, 50_000),
            user(4),
            assistant_plain(5),
        ];
        let keep = safe_tail_start(&records, 2);
        assert!(keep > 0 && keep < records.len());
        assert_eq!(compaction_cutoff(&records, 2, 100), Some(keep));
    }

    // -------------------------------------------------------------
    // ToolCallGate
    // -------------------------------------------------------------

    fn gate_config(
        turn_limit: usize,
        rate: Option<RateLimit>,
        session_max: Option<usize>,
    ) -> AgentConfig {
        AgentConfig {
            session_name: String::new(),
            model: String::new(),
            max_tokens: None,
            workspace_root: PathBuf::new(),
            system_prompt: None,
            show_reasoning: false,
            max_turns: 0,
            mode: Mode::Default,
            extra_read_paths_cli: Vec::new(),
            reference_paths: Vec::new(),
            turn_tool_call_limit: turn_limit,
            tool_call_rate: rate,
            session_tool_call_max: session_max,
            skill_name: None,
            authorization: Authorization::PerTool,
        }
    }

    #[test]
    fn gate_turn_limit_admits_up_to_boundary_then_rejects() {
        let cfg = gate_config(3, None, None);
        let mut gate = ToolCallGate::new(&cfg);
        gate.begin_turn();
        let now = Instant::now();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::TurnLimitExceeded);
    }

    #[test]
    fn gate_turn_limit_resets_after_begin_turn() {
        let cfg = gate_config(2, None, None);
        let mut gate = ToolCallGate::new(&cfg);
        let now = Instant::now();
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::TurnLimitExceeded);
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
    }

    #[test]
    fn gate_rate_admits_up_to_boundary_within_window_then_rejects() {
        let cfg = gate_config(
            100,
            Some(RateLimit {
                calls: 2,
                window: Duration::from_secs(10),
            }),
            None,
        );
        let mut gate = ToolCallGate::new(&cfg);
        gate.begin_turn();
        let t0 = Instant::now();
        assert_eq!(gate.admit(t0), GateDecision::Proceed);
        assert_eq!(gate.admit(t0), GateDecision::Proceed);
        assert_eq!(gate.admit(t0), GateDecision::RateLimitExceeded);
    }

    #[test]
    fn gate_rate_window_slides_and_admits_again() {
        let cfg = gate_config(
            100,
            Some(RateLimit {
                calls: 2,
                window: Duration::from_secs(10),
            }),
            None,
        );
        let mut gate = ToolCallGate::new(&cfg);
        gate.begin_turn();
        let t0 = Instant::now();
        assert_eq!(gate.admit(t0), GateDecision::Proceed);
        assert_eq!(gate.admit(t0), GateDecision::Proceed);
        assert_eq!(gate.admit(t0), GateDecision::RateLimitExceeded);
        // Advance well past the window; the deque should drain.
        let t1 = t0 + Duration::from_secs(11);
        assert_eq!(gate.admit(t1), GateDecision::Proceed);
    }

    #[test]
    fn gate_rate_rejection_does_not_consume_window_slot() {
        // If a rejected call filled the deque, the next call after
        // window sliding would immediately be rejected again. Verify
        // rejected calls are not pushed.
        let cfg = gate_config(
            100,
            Some(RateLimit {
                calls: 1,
                window: Duration::from_secs(10),
            }),
            None,
        );
        let mut gate = ToolCallGate::new(&cfg);
        gate.begin_turn();
        let t0 = Instant::now();
        assert_eq!(gate.admit(t0), GateDecision::Proceed);
        // Multiple rejections at t0 must not affect anything.
        assert_eq!(gate.admit(t0), GateDecision::RateLimitExceeded);
        assert_eq!(gate.admit(t0), GateDecision::RateLimitExceeded);
        // After the window slides, exactly one admit is possible.
        let t1 = t0 + Duration::from_secs(11);
        assert_eq!(gate.admit(t1), GateDecision::Proceed);
        assert_eq!(gate.admit(t1), GateDecision::RateLimitExceeded);
    }

    #[test]
    fn gate_session_backstop_admits_up_to_max_then_exhausts() {
        let cfg = gate_config(100, None, Some(3));
        let mut gate = ToolCallGate::new(&cfg);
        let now = Instant::now();
        // Session count spans multiple turns.
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        assert_eq!(gate.admit(now), GateDecision::SessionExhausted);
    }

    #[test]
    fn gate_none_options_disable_the_check() {
        let cfg = gate_config(100, None, None);
        let mut gate = ToolCallGate::new(&cfg);
        gate.begin_turn();
        let now = Instant::now();
        for _ in 0..50 {
            assert_eq!(gate.admit(now), GateDecision::Proceed);
        }
    }

    #[test]
    fn gate_rejection_does_not_consume_session_or_turn_budget() {
        // Turn cap rejects, but session_count and rate_deque should
        // not have advanced by the rejected call.
        let cfg = gate_config(
            1,
            Some(RateLimit {
                calls: 100,
                window: Duration::from_secs(10),
            }),
            Some(3),
        );
        let mut gate = ToolCallGate::new(&cfg);
        let now = Instant::now();
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        // Rejected by turn cap; must not count against session_max.
        assert_eq!(gate.admit(now), GateDecision::TurnLimitExceeded);
        assert_eq!(gate.admit(now), GateDecision::TurnLimitExceeded);
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::Proceed);
        // Now session_count == 3, backstop rejects (turn cap would
        // also apply on the 2nd of this turn but session runs first
        // per the order).
        gate.begin_turn();
        assert_eq!(gate.admit(now), GateDecision::SessionExhausted);
    }

    // -------------------------------------------------------------
    // resolve_references
    // -------------------------------------------------------------

    fn temp_ref_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("attini-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_references_inlines_small_utf8_file() {
        let dir = temp_ref_dir("ref-small");
        let path = dir.join("a.md");
        std::fs::write(&path, "hello reference\n").unwrap();

        let refs = resolve_references(&dir, std::slice::from_ref(&path)).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].abs_path, std::fs::canonicalize(&path).unwrap());
        assert_eq!(refs[0].inline.as_deref(), Some("hello reference\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_references_marks_oversized_file_as_opaque() {
        let dir = temp_ref_dir("ref-big");
        let path = dir.join("big.md");
        let big = "x".repeat(REFERENCE_MAX_BYTES + 1);
        std::fs::write(&path, &big).unwrap();

        let refs = resolve_references(&dir, std::slice::from_ref(&path)).unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].inline.is_none());
        assert_eq!(refs[0].abs_path, std::fs::canonicalize(&path).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_references_resolves_relative_path_against_workspace() {
        let dir = temp_ref_dir("ref-rel");
        std::fs::write(dir.join("rel.md"), "relative\n").unwrap();

        let refs = resolve_references(&dir, &[std::path::PathBuf::from("rel.md")]).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].inline.as_deref(), Some("relative\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_references_missing_file_errors() {
        let dir = temp_ref_dir("ref-miss");
        let err = resolve_references(&dir, &[dir.join("nope.md")]).unwrap_err();
        assert!(err.to_string().contains("--reference"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // pick_summary_text
    // -------------------------------------------------------------

    fn call_result(content: &str, reasoning: Option<&str>) -> curl::CallResult {
        curl::CallResult {
            content: content.to_string(),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }
    }

    #[test]
    fn pick_summary_text_returns_content_when_present() {
        let r = call_result("hello summary", Some("thinking..."));
        assert_eq!(pick_summary_text(&r), Some("hello summary".to_string()));
    }

    #[test]
    fn pick_summary_text_falls_back_to_reasoning_when_content_empty() {
        let r = call_result("   ", Some("actual answer"));
        assert_eq!(pick_summary_text(&r), Some("actual answer".to_string()));
    }

    #[test]
    fn pick_summary_text_returns_none_when_reasoning_is_some_empty() {
        let r = call_result("", Some("   "));
        assert_eq!(pick_summary_text(&r), None);
    }

    #[test]
    fn pick_summary_text_returns_none_when_reasoning_is_none() {
        let r = call_result("", None);
        assert_eq!(pick_summary_text(&r), None);
    }

    // -----------------------------------------------------------------
    // abbreviate_args / render_records_prose
    // -----------------------------------------------------------------

    #[test]
    fn abbreviate_args_strips_newlines_and_truncates() {
        let long = "{\"path\":\"src/agent_cli.rs\",\n\"pattern\":\"".repeat(40);
        let out = abbreviate_args(&long);
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= 91); // 90 + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn abbreviate_args_handles_empty() {
        assert_eq!(abbreviate_args(""), "");
        assert_eq!(abbreviate_args("   "), "");
    }

    #[test]
    fn render_records_prose_strips_raw_tool_format() {
        let records = vec![
            user(1),
            ChatMessageWithTs {
                message: ChatMessage::Assistant {
                    content: "looking at the file".to_string(),
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        function_name: "search".to_string(),
                        arguments_json: "{\"pattern\":\"foo\"}".to_string(),
                    }],
                },
                ts: 2,
            },
            ChatMessageWithTs {
                message: ChatMessage::Tool {
                    tool_call_id: "call_1".to_string(),
                    content: "a file
"
                    .repeat(300),
                },
                ts: 3,
            },
        ];
        let out = render_records_prose(&records);
        assert!(out.contains("user: u1"));
        assert!(out.contains("assistant: looking at the file"));
        assert!(out.contains("[tool call: search ({\"pattern\":\"foo\"})]"));
        assert!(out.contains("[tool result: a file"));
        // Raw JSON / agentic XML must not leak into the rendered prose.
        assert!(!out.contains("tool_calls"));
        assert!(!out.contains("<invoke"));
        assert!(!out.contains("function_name"));
        // Tool result is truncated (~200-char brief), not the full ~2100 chars.
        assert!(out.len() < 600);
        assert!(out.contains('…'));
    }

    // -----------------------------------------------------------------
    // command_result_json
    // -----------------------------------------------------------------

    #[test]
    fn command_result_json_keeps_full_output_text() {
        let stdout = "line 1\nline 2\n".repeat(200);
        let stderr = "warning: something\n".repeat(50);
        let json = command_result_json(
            &stdout,
            &stderr,
            Some(1),
            "exited",
            Duration::from_millis(123),
        );
        assert!(json.contains("\"stdout\":\"line 1\\nline 2\\n"));
        assert!(json.contains("\"stderr\":\"warning: something\\n"));
        assert!(json.contains("\"exit_code\":1"));
        assert!(json.contains("\"termination_reason\":\"exited\""));
        assert!(json.contains("\"duration_ms\":123"));
    }

    #[test]
    fn command_result_json_roundtrips_no_exit_code() {
        let json = command_result_json("out", "", None, "signaled", Duration::ZERO);
        assert!(json.contains("\"exit_code\":null"));
        assert!(json.contains("\"termination_reason\":\"signaled\""));
    }

    // -------------------------------------------------------------
    // repair_messages (transcript integrity)
    // -------------------------------------------------------------

    fn assistant_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage::Assistant {
            content: String::new(),
            reasoning_content: None,
            tool_calls: ids
                .iter()
                .map(|id| ToolCall {
                    id: id.to_string(),
                    function_name: "command".to_string(),
                    arguments_json: "{}".to_string(),
                })
                .collect(),
        }
    }

    fn tool_result(call_id: &str) -> ChatMessage {
        ChatMessage::Tool {
            tool_call_id: call_id.to_string(),
            content: "{}".to_string(),
        }
    }

    #[test]
    fn repair_messages_inserts_reject_for_unanswered_sibling() {
        // assistant issues two calls; only the first is answered. The
        // second must get a synthetic reject inserted after the first
        // tool result, preserving order.
        let messages = vec![
            assistant_calls(&["call_00", "call_01"]),
            tool_result("call_00"),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].call_id, "call_01");
        assert!(orphans[0].content.contains("unanswered_tool_call"));
        // Two assistants? No: assistant + two tool messages.
        assert_eq!(repaired.len(), 3);
        assert!(matches!(&repaired[0], ChatMessage::Assistant { .. }));
        match &repaired[1] {
            ChatMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "call_00"),
            other => panic!("expected tool call_00, got {other:?}"),
        }
        match &repaired[2] {
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "call_01");
                assert!(content.contains("unanswered_tool_call"));
            }
            other => panic!("expected synthetic tool call_01, got {other:?}"),
        }
    }

    #[test]
    fn repair_messages_leaves_complete_transcript_untouched() {
        let messages = vec![
            assistant_calls(&["call_00", "call_01"]),
            tool_result("call_00"),
            tool_result("call_01"),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert!(orphans.is_empty());
        assert_eq!(repaired, messages);
    }

    #[test]
    fn repair_messages_handles_multiple_unanswered_calls() {
        // No tool results at all: every call is answered by a synthetic
        // reject, all inserted after the assistant message.
        let messages = vec![assistant_calls(&["call_00", "call_01", "call_02"])];
        let (repaired, orphans) = repair_messages(&messages);
        assert_eq!(orphans.len(), 3);
        assert_eq!(repaired.len(), 4);
        for (idx, id) in ["call_00", "call_01", "call_02"].iter().enumerate() {
            match &repaired[idx + 1] {
                ChatMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, id),
                other => panic!("expected tool {id}, got {other:?}"),
            }
        }
    }

    #[test]
    fn repair_messages_places_synthetic_before_next_user() {
        // The synthetic reject must be inserted immediately after the
        // assistant's tool results, before a subsequent user message.
        let messages = vec![
            assistant_calls(&["call_00", "call_01"]),
            tool_result("call_00"),
            ChatMessage::User("continue".to_string()),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert_eq!(orphans.len(), 1);
        assert_eq!(repaired.len(), 4);
        assert!(matches!(&repaired[2], ChatMessage::Tool { .. }));
        assert!(matches!(&repaired[3], ChatMessage::User(_)));
    }

    #[test]
    fn repair_messages_skips_assistant_without_tool_calls() {
        let messages = vec![
            ChatMessage::assistant_text("intro"),
            assistant_calls(&["call_00"]),
            tool_result("call_00"),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert!(orphans.is_empty());
        assert_eq!(repaired, messages);
    }

    #[test]
    fn repair_messages_moves_misplaced_tool_before_user() {
        // The bug this fix addresses: a synthetic tool result was
        // persisted *after* a user turn. It must be moved back to
        // immediately follow its assistant so the assistant -> tool
        // continuity holds, with the user message after the tool
        // results, and no extra orphan synthetic is generated.
        let messages = vec![
            assistant_calls(&["call_00", "call_01"]),
            tool_result("call_00"),
            ChatMessage::User("tudukete".to_string()),
            tool_result("call_01"),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert!(orphans.is_empty());
        assert_eq!(repaired.len(), 4);
        assert!(matches!(&repaired[0], ChatMessage::Assistant { .. }));
        match &repaired[1] {
            ChatMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "call_00"),
            other => panic!("expected tool call_00, got {other:?}"),
        }
        match &repaired[2] {
            ChatMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "call_01"),
            other => panic!("expected tool call_01, got {other:?}"),
        }
        assert!(matches!(&repaired[3], ChatMessage::User(_)));
    }

    #[test]
    fn repair_messages_drops_duplicate_tool_result() {
        // A stray / duplicate tool message for an already-answered call
        // is dropped rather than re-emitted.
        let messages = vec![
            assistant_calls(&["call_00"]),
            tool_result("call_00"),
            tool_result("call_00"),
        ];
        let (repaired, orphans) = repair_messages(&messages);
        assert!(orphans.is_empty());
        assert_eq!(repaired.len(), 2);
        assert!(matches!(&repaired[0], ChatMessage::Assistant { .. }));
        assert!(matches!(&repaired[1], ChatMessage::Tool { .. }));
    }

    // -------------------------------------------------------------
    // render_agent_status_line
    // -------------------------------------------------------------

    #[test]
    fn status_line_render_includes_session_and_ctx() {
        let line = render_agent_status_line("deepseek-v4-flash", "main", 20736);
        assert_eq!(
            line,
            "[agent] model=deepseek-v4-flash session=main ctx=20736"
        );
    }

    #[test]
    fn status_line_uses_passed_ctx_as_current_size() {
        let line = render_agent_status_line("m", "s", 1000);
        assert!(line.contains("ctx=1000"));
        assert!(line.contains("model=m"));
        assert!(line.contains("session=s"));
    }
}
