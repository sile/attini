//! Sync single-turn agent loop for `attini tell`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use nojson::{DisplayJson, RawJson};

use crate::curl::{self, ProgressSinks};
use crate::permissions;
use crate::sansio::agent::{
    CommandError, CommandInvocation, PatchInvocation, PatchPreview, PatchTool, ReadOnlyTool,
    ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall, ToolDef};
use crate::sansio::permissions::{
    Authorization, AutoDecision, Judgment, Rule, RuleScope, ScopedRules, evaluate, evaluate_read,
    evaluate_write,
};
use crate::session::{
    ApprovalDecision, AutoDecidedBy, AutoDecidedMatch, ChatMessageWithTs, InvocationEndReason,
    MetricsSnapshotBody, Pending, PendingToolKind, Session, SessionRecord, TokenUsageBody,
    now_unix_millis,
};
use crate::tools::ToolExecutor;

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_AWAITING_APPROVAL: u8 = 10;

pub const DEFAULT_MAX_TURNS: usize = 20;

/// Counters collected during one invocation of `tell_cli::run` for
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
    /// of the various `Err` outcomes.)
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

/// Upper bound on the number of diff lines printed for a patch
/// preview / auto-approved patch. Beyond this the rest is collapsed
/// into a `... (N more lines omitted)` marker. Keeps a pathological
/// patch from flooding the terminal while still showing what changed
/// for the common case.
pub const PATCH_PREVIEW_MAX_LINES: usize = 200;

/// Upper bound on the number of content lines printed for a `read`
/// tool result on stderr. Beyond this the rest is collapsed into a
/// `... (N more lines omitted)` marker, so a large read cannot flood
/// the terminal while still showing the head of what was read. The
/// JSON payload sent to the model is unaffected; this is display
/// only.
pub const READ_PREVIEW_MAX_LINES: usize = 20;

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

/// Maximum raw character size of the real records between the last
/// summary and now, used as a second auto-compaction trigger. When
/// the previous turn suspended before recording its `token_usage`,
/// `latest_prompt_tokens()` is stale/small even though the actual
/// records (which include a huge tool result) are enormous; this
/// bound catches that case so compaction still fires. Mirrors
/// [`RETAINED_TAIL_MAX_CHARS`] for consistency.
pub const RECORDS_TOTAL_MAX_CHARS: usize = 250_000;

/// Byte size of `conversation.jsonl` at which an automatic physical
/// prune pass runs. Compaction appends a summary but never deletes the
/// records it summarised, so the append-only log grows without bound;
/// once it crosses this size the records before the midpoint are
/// dropped at a safe boundary. 100 MB is far larger than any session
/// that still benefits from full history, so this fires rarely.
pub const CONVERSATION_PRUNE_TRIGGER_BYTES: u64 = 100 * 1024 * 1024;

pub struct TellConfig {
    pub session_name: String,
    pub model: String,
    /// Maximum completion tokens per model call. `None` uses the
    /// model's own default; `Some(n)` caps response size / cost.
    pub max_tokens: Option<u64>,
    pub workspace_root: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: usize,
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
    /// How side-effecting tool calls are authorized in this
    /// invocation. `plan run` supplies
    /// [`Authorization::ApprovedPlan`]; every other entry point uses
    /// the default `PerTool`.
    pub authorization: Authorization,
    /// Sampling temperature for model calls. `None` uses the request
    /// default (`Some(0.0)`, deterministic code editing); `Some(t)`
    /// overrides it.
    pub temperature: Option<f64>,
    /// Requested one-shot grant to run alongside a `Continuation::Approve`:
    /// `attini approve --grant <SCOPE>`. Persists an auto-approve rule for
    /// the approved command's argv-prefix after the approval succeeds.
    /// `None` for every other entry point.
    pub grant_request: GrantRequest,
    /// Wall-clock cap on a single `command` tool call, in seconds. The
    /// child runs in its own process group and is killed (SIGTERM, then
    /// SIGKILL) when the cap elapses; the result sets `termination_reason`
    /// to `timeout`. `None` disables the cap. `Some(0)` is treated as
    /// disabled too, so `--command-timeout 0` opts out.
    pub command_timeout_seconds: Option<u64>,
}

/// Default `command` tool timeout in seconds, used when neither
/// `--command-timeout` nor `ATTINI_COMMAND_TIMEOUT_SECONDS` is set.
pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 180;

/// The `--grant SCOPE` value accepted by `attini approve`.
///
/// `Oneshot` is the default: approve the pending call and persist
/// nothing. `Session` / `Workspace` additionally append the approved
/// command's argv-prefix as an auto-approve rule to the corresponding
/// `permissions.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRequest {
    /// No grant was requested (`--grant` omitted, or not an approve run).
    None,
    /// `--grant oneshot`: persist nothing (the default approve behavior).
    Oneshot,
    /// `--grant session`: append to the session-local `permissions.jsonl`.
    Session,
    /// `--grant workspace`: append to the workspace-wide `permissions.jsonl`.
    Workspace,
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
    /// Resume a stopped session: approve the pending tool call in the
    /// session's `pending.json` if there is one; otherwise re-issue the
    /// identical request when the previous invocation ended in a
    /// retryable transport failure ([`Continuation::Retry`]); otherwise
    /// continue with a fixed continuation message ([`RESUME_PROMPT`]).
    Approve,
    /// Internal only: re-run the loop against the conversation exactly
    /// as it stands, without appending any new user record. Produced by
    /// normalising [`Continuation::Approve`] when the previous
    /// invocation ended in [`InvocationEndReason::TransportError`] before
    /// any assistant output was recorded, so the same request can safely
    /// be sent again.
    Retry,
}

/// Fixed user message appended when `attini approve` is run on a session
/// that has no pending tool call (i.e. it stopped at `max_turns`). It
/// deliberately carries no new instruction: approving a stop means "keep
/// going", while a new instruction goes through `attini tell`. The model
/// already has its own last turn in context, so a bare continuation is
/// enough to pick the work back up.
pub const RESUME_PROMPT: &str = "Continue from where you left off.";

/// Terminal outcome of one `tell_cli::run` invocation.
#[derive(Debug)]
pub enum TellOutcome {
    /// Normal exit with the process exit code.
    Exit(ExitCode),
}

pub fn run(cfg: TellConfig, cont: Continuation) -> io::Result<TellOutcome> {
    let mut session = Session::open(&cfg.session_name)?;
    // Canonicalise the persistent extra_read_paths (from
    // permissions.jsonl) and hand the resulting Vec to the
    // ToolExecutor. Any path that fails
    // to canonicalise is warned + skipped so a single bad entry does
    // not disable the whole read-path list.
    let loaded = permissions::load(&cfg.session_name)?;
    let candidates: Vec<PathBuf> = loaded.extra_read_paths.iter().map(PathBuf::from).collect();
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
            render_tell_status_line(&cfg.model, &cfg.session_name, ctx_tokens)
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
        Ok(Driven::TransportFailed(_)) => (InvocationEndReason::TransportError, EXIT_ERROR),
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
        Ok(Driven::TransportFailed(message)) => {
            eprintln!("attini: {message}");
            eprintln!(
                "attini: this looks like a transient transport failure; run \
                 `attini approve -s {}` to re-issue the same request",
                cfg.session_name
            );
            Ok(TellOutcome::Exit(ExitCode::from(EXIT_ERROR)))
        }
        Ok(_) => Ok(TellOutcome::Exit(ExitCode::from(exit_code))),
        Err(e) => {
            eprintln!("attini: {e}");
            Ok(TellOutcome::Exit(ExitCode::from(EXIT_ERROR)))
        }
    }
}

/// Render the single-line start-of-invocation breadcrumb written to stderr
/// by [`run`]. Pure function so the format can be unit-tested without
/// touching stdout. `ctx=` is the current conversation size (the last
/// recorded `prompt_tokens`) handed in by the caller; it is not the
/// cumulative billed total.
fn render_tell_status_line(model: &str, session_name: &str, ctx_tokens: u64) -> String {
    format!(
        "[tell] model={} session={} ctx={}",
        model, session_name, ctx_tokens,
    )
}

enum Driven {
    Completed,
    AwaitingApproval,
    /// Invocation-scope tool-call backstop tripped
    /// ([`TellConfig::session_tool_call_max`]).
    SessionToolCallExhausted,
    /// A model call failed at the transport layer before any assistant
    /// output for the turn was recorded. Recorded as
    /// [`InvocationEndReason::TransportError`] so a later `attini
    /// approve` can re-issue the request. Carries the human-readable
    /// failure message for the stderr diagnostic.
    TransportFailed(String),
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
    fn new(cfg: &TellConfig) -> Self {
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

/// Map a pending-free `attini approve` to the continuation it should
/// actually run, given the reason the previous invocation ended.
///
/// A retryable transport failure re-issues the identical request
/// ([`Continuation::Retry`]); anything else falls back to the fixed
/// continuation message. Pure so the decision can be unit-tested
/// without a session.
fn normalise_pending_free_approve(last: Option<InvocationEndReason>) -> Continuation {
    if last == Some(InvocationEndReason::TransportError) {
        Continuation::Retry
    } else {
        Continuation::Prompt(RESUME_PROMPT.to_string())
    }
}

fn drive(
    session: &mut Session,
    executor: &ToolExecutor,
    cfg: &TellConfig,
    cont: Continuation,
    counters: &mut Counters,
) -> io::Result<Driven> {
    // `Approve` resumes a stopped session. Normalise the no-pending
    // cases here so the rest of `drive` stays single-path:
    //   1. pending tool call present  -> approve + execute (unchanged).
    //   2. else, previous invocation ended in a retryable transport
    //      failure -> re-issue the identical request
    //      ([`Continuation::Retry`]); nothing new is appended.
    //   3. else (stopped at `max_turns`) -> fixed continuation message.
    let cont = match cont {
        Continuation::Approve if session.load_pending()?.is_some() => Continuation::Approve,
        Continuation::Approve => {
            let last = session.last_invocation_end_reason()?;
            if last == Some(InvocationEndReason::TransportError) {
                eprintln!(
                    "[approve] previous invocation ended in a transport error; re-issuing the same request"
                );
            }
            normalise_pending_free_approve(last)
        }
        other => other,
    };

    if matches!(cont, Continuation::Prompt(_)) {
        try_auto_compact(session, &cfg.model, counters, cfg.max_tokens)?;
    }

    let is_prompt = matches!(cont, Continuation::Prompt(_));
    let is_retry = matches!(cont, Continuation::Retry);
    let is_approve = matches!(cont, Continuation::Approve);

    let mut messages = build_initial_messages(session, cfg)?;

    // The `Prompt` path appends a fresh user record before the model
    // call; `Retry` re-issues the existing conversation as-is. Any
    // assistant `tool_call` left unanswered when the loop previously
    // suspended must be answered *before* that user record is appended
    // (or before the identical request is re-sent), otherwise a
    // synthetic tool result would be persisted after the user and break
    // the assistant -> tool continuity the API requires. A transport
    // error leaves no assistant record, so `Retry` normally repairs
    // nothing, but the pass is harmless and keeps the invariant.
    if is_prompt || is_retry {
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
            // `--grant` is validated here (after the pending set is known)
            // but applied only after every approval has succeeded, so a
            // rejected/broken pending set never leaves a grant behind. See
            // [`plan_grant`] for the up-front checks.
            let grant = plan_grant(cfg.grant_request, &pendings, executor.root())?;
            for pending in &pendings {
                session.append(&SessionRecord::ToolApproval {
                    ts: now_unix_millis(),
                    call_id: pending.call_id.clone(),
                    decision: ApprovalDecision::Approve,
                    auto_decided_by: None,
                })?;
                let content = execute_pending(pending, executor, command_timeout(cfg))?;
                append_tool(session, &mut messages, &pending.call_id, content)?;
            }
            session.clear_pending()?;
            // Best-effort: the approval already stands, so a grant failure
            // is a warning, not a rollback.
            if let Some(intent) = grant {
                apply_grant(cfg, &intent);
            }
        }
        // Re-issue the identical request: append no new user record, just
        // fall through to the model-call loop with the conversation as-is.
        Continuation::Retry => {}
    }

    // `Approve` consumes the parked pending inside the match above
    // (appending a Tool record for the answered call), so it runs the
    // orphan repair *after* the match; otherwise the pending is still
    // parked and the freshly-appended Tool record could be re-surfaced
    // as an orphan.
    if is_approve {
        let repaired = repair_orphaned_tool_calls(session, &mut messages)?;
        if repaired > 0 {
            eprintln!(
                "[repair] inserted {repaired} synthetic tool result(s) for unanswered tool_call(s)"
            );
        }
    }

    let tools = build_tool_defs();
    let rules = permissions::load(&cfg.session_name)?;
    // Rule chain in increasing precedence: workspace, then session.
    let permission_layers: Vec<(RuleScope, &[Rule])> = vec![
        (RuleScope::Workspace, rules.workspace.as_slice()),
        (RuleScope::Session, rules.session.as_slice()),
    ];
    let mut gate = ToolCallGate::new(cfg);

    for _ in 0..cfg.max_turns {
        gate.begin_turn();
        let request = ChatRequest::new(cfg.model.clone(), messages.clone())
            .with_tools(tools.clone())
            .with_max_tokens(cfg.max_tokens)
            .with_temperature(cfg.temperature);
        let mut stdout = io::stdout();
        let call_result = {
            let mut sinks = ProgressSinks {
                content: &mut stdout,
            };
            match curl::call(&request, &mut sinks) {
                Ok(r) => r,
                Err(e) if e.is_retryable() => {
                    // Transport fault: the turn produced no assistant
                    // output, so the identical request can be re-issued
                    // by `attini approve`. Record it as a distinct end
                    // reason and stop cleanly (exit 1) rather than
                    // surfacing a raw error.
                    return Ok(Driven::TransportFailed(format!("model call failed: {e}")));
                }
                Err(e) => {
                    return Err(io::Error::other(format!("model call failed: {e}")));
                }
            }
        };
        let _ = writeln!(io::stdout());

        let assistant = call_result.clone().into_assistant();
        session.append(&SessionRecord::Assistant {
            ts: now_unix_millis(),
            content: call_result.content.clone(),
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
                        match run_read_only(tc, executor, &permission_layers, &cfg.authorization) {
                            ReadOnlyDispatch::Done {
                                summary,
                                content,
                                errored,
                            } => {
                                eprintln!("{summary}");
                                if errored {
                                    counters.tool_errors += 1;
                                }
                                append_tool(session, &mut messages, &tc.id, content)?;
                            }
                            ReadOnlyDispatch::NeedsApproval { summary, preview } => {
                                eprintln!("{summary}");
                                parked.push(build_pending(tc, PendingToolKind::Read, preview));
                                suspending = true;
                            }
                        }
                    }
                }
                ToolKind::Patch => {
                    if let PatchDispatch::Awaiting(pending) = dispatch_patch_unapproved(
                        tc,
                        executor,
                        &permission_layers,
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
                ToolKind::Command => {
                    if let CommandDispatch::Awaiting(pending) = dispatch_command(
                        tc,
                        executor,
                        &permission_layers,
                        &cfg.authorization,
                        session,
                        &mut messages,
                        counters,
                        suspending,
                        command_timeout(cfg),
                    )? {
                        parked.push(pending);
                        suspending = true;
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

    Err(io::Error::other(max_turns_error(cfg.max_turns)))
}

/// Build the error message shown when `tell` runs out of turns. The
/// continuation command is placed on its own line so it can be copied
/// verbatim; the session is taken from `-s` / `ATTINI_SESSION_NAME`
/// (defaulting to `main`), so it is omitted here rather than restating
/// a name that is already implicit in context. Exit code stays the
/// generic 1 (a `tell` loop that used all its turns is a runtime
/// failure, not a success).
fn max_turns_error(max_turns: usize) -> String {
    format!(
        "tell loop exceeded max_turns={max_turns}; continue this session? \
         run the following command:\n\
         attini approve  # or give a new instruction with: attini tell '...'"
    )
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

fn build_initial_messages(session: &Session, cfg: &TellConfig) -> io::Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
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
    messages.push(ChatMessage::System(render_scratchpad_note(
        &cfg.session_name,
    )));
    messages.push(ChatMessage::System(render_tool_batching_note()));
    for record in session.load_records_since_last_summary()? {
        messages.push(record.message);
    }
    Ok(messages)
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

/// Tell the model how to batch tool calls within a single turn so a
/// read-only call is not stranded behind an approval-gated one. When
/// a turn emits a call that needs approval (a `command`, or a `patch`
/// on a non-tracked path), any tool call ordered after it in the same
/// turn is left unanswered and later cancelled by the orphan-repair
/// pass — the model receives no result for it and must reissue it.
/// Emitting approval-gated calls last (or alone) avoids the wasted
/// round trip.
fn render_tool_batching_note() -> String {
    "# Tool call batching\n\n\
     You may emit several tool calls in one turn. However, if any of them \
     requires human approval — a `command`, or a `patch` on a non-tracked \
     path — place it **last** in the turn, or emit it alone. Any tool call \
     ordered after an approval-gated one (including a read-only `read`, \
     `search`, or `list`) is left unanswered and cancelled on resume, so \
     you would have to reissue it. Read-only calls may be freely batched \
     together, and may precede an approval-gated call; just do not put \
     them after one.\n"
        .to_string()
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

/// Decide whether to auto-compact before a `Prompt` invocation.
///
/// Returns `true` when at least one of two independent signals says
/// the real history is too large:
///   * the last successful turn recorded `>= COMPACTION_TRIGGER_TOKENS`
///     prompt tokens (`latest`);
///   * the raw character size of the real records since the last
///     summary (`total_chars`) exceeds `RECORDS_TOTAL_MAX_CHARS`.
///
/// The second signal matters when the previous turn suspended before
/// recording its `token_usage`: `latest` is then stale/small, but the
/// actual records (e.g. a huge tool result) are still enormous and
/// would overflow the next main call.
fn should_auto_compact(latest: u64, total_chars: usize) -> bool {
    latest >= COMPACTION_TRIGGER_TOKENS || total_chars > RECORDS_TOTAL_MAX_CHARS
}

fn try_auto_compact(
    session: &mut Session,
    model: &str,
    counters: &mut Counters,
    max_tokens: Option<u64>,
) -> io::Result<()> {
    if session.load_pending()?.is_some() {
        return Ok(());
    }
    // Physical pruning is independent of summarisation: it fires purely
    // on file size, so it must be checked even when the records since
    // the last summary are already small. It runs while the session
    // LOCK is held, so the log is only rewritten by its owner.
    if let Err(e) = maybe_prune_conversation(session) {
        eprintln!("[prune] skipped: {e}");
    }
    // Judge the need to compact from two independent signals:
    //   * the token threshold, which reflects the *last successful* turn;
    //   * the raw size of the real records since the last summary, which
    //     stays accurate even when that turn suspended before recording
    //     `token_usage` (leaving a huge tool result behind but a stale,
    //     small `latest`).
    let latest = session.latest_prompt_tokens()?.unwrap_or(0);
    let total_chars = if latest < COMPACTION_TRIGGER_TOKENS {
        let records = session.load_records_since_last_summary()?;
        records
            .iter()
            .map(|r| message_raw_char_len(&r.message) + 1)
            .sum::<usize>()
    } else {
        0
    };
    if !should_auto_compact(latest, total_chars) {
        return Ok(());
    }
    if latest < COMPACTION_TRIGGER_TOKENS {
        eprintln!(
            "[compaction] previous prompt was {latest} tokens (below threshold) but records are \
             {total_chars} chars, summarising..."
        );
    } else {
        eprintln!(
            "[compaction] previous prompt was {latest} tokens (threshold {COMPACTION_TRIGGER_TOKENS}), summarising..."
        );
    }
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
/// Called by the auto-compaction path (`try_auto_compact`). Callers are
/// expected to have already checked that the session is idle (no LOCK
/// holder, no `pending.json`).
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

/// Automatic physical pruning: when `conversation.jsonl` has grown past
/// [`CONVERSATION_PRUNE_TRIGGER_BYTES`], drop every record before the
/// first safe boundary at or after the byte midpoint, roughly halving
/// the file. There is no manual `prune` command; this is the only path.
///
/// Called from `try_auto_compact` while the session `LOCK` is held, so
/// the log is only ever rewritten by the process that owns the session.
/// Returns `Ok(None)` when the file is under the threshold or no safe
/// boundary exists past the midpoint (in which case the file is left
/// untouched rather than split a pair).
fn maybe_prune_conversation(session: &Session) -> io::Result<Option<PruneStats>> {
    let path = session.conversation_path();
    let orig_size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if orig_size <= CONVERSATION_PRUNE_TRIGGER_BYTES {
        return Ok(None);
    }
    let Some((offset, dropped)) = prune_offset_past_midpoint(path, orig_size)? else {
        return Ok(None);
    };
    if offset == 0 {
        return Ok(None);
    }
    rewrite_file_from_offset(path, offset)?;
    let new_size = std::fs::metadata(path)?.len();
    eprintln!(
        "[prune] dropped {dropped} records, {orig_size} -> {new_size} bytes (file crossed \
         {CONVERSATION_PRUNE_TRIGGER_BYTES} bytes)"
    );
    Ok(Some(PruneStats {
        dropped_records: dropped,
        orig_size,
        new_size,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PruneStats {
    dropped_records: u64,
    orig_size: u64,
    new_size: u64,
}

/// Scan `conversation.jsonl` and return the byte offset of the first
/// *safe* record boundary at or after `orig_size / 2`, together with
/// the number of records strictly before it.
///
/// A safe boundary is a line whose message is a User record or an
/// Assistant record without pending `tool_calls` (see
/// [`is_safe_boundary`]); cutting there never separates an
/// `assistant -> tool` pair. Returns `Ok(None)` when no such boundary
/// exists at or after the midpoint (the whole tail is one unresolved
/// pair), so the caller leaves the file alone.
fn prune_offset_past_midpoint(
    path: &std::path::Path,
    orig_size: u64,
) -> io::Result<Option<(u64, u64)>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let midpoint = orig_size / 2;
    let mut offset: u64 = 0;
    let mut line_index: u64 = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let start = offset;
        let read_bytes = reader.read_line(&mut line)?;
        if read_bytes == 0 {
            break;
        }
        offset += read_bytes as u64;
        if !line.trim().is_empty() {
            if start >= midpoint && line_is_safe_boundary(&line) {
                return Ok(Some((start, line_index)));
            }
            line_index += 1;
        }
    }
    Ok(None)
}

/// Whether a raw conversation line is a safe prune boundary: it parses
/// as a `user` record, or as an `assistant` record whose `tool_calls`
/// are empty. Anything else (`tool`, an assistant turn awaiting tools,
/// a non-message record) is unsafe to cut at.
fn line_is_safe_boundary(line: &str) -> bool {
    let json = match RawJson::parse(line) {
        Ok(j) => j,
        Err(_) => return false,
    };
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str());
    match kind {
        Ok(ref k) if k.as_ref() == "user" => true,
        Ok(ref k) if k.as_ref() == "assistant" => {
            // Safe when it carries non-empty `text` (a final answer).
            let has_text = value
                .to_member("text")
                .and_then(|m| m.required())
                .ok()
                .and_then(|t| t.to_unquoted_string_str().ok())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if has_text {
                return true;
            }
            // Otherwise safe only when it carries no pending tool calls.
            let has_calls = value
                .to_member("tool_calls")
                .and_then(|m| m.required())
                .ok()
                .and_then(|tc| tc.to_array().ok())
                .map(|mut a| a.next().is_some())
                .unwrap_or(false);
            !has_calls
        }
        _ => false,
    }
}

/// Copy the suffix of `path` starting at `offset` over the whole file,
/// via a tmp file + rename so a crash mid-write cannot truncate the
/// conversation.
fn rewrite_file_from_offset(path: &std::path::Path, offset: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    drop(file);

    let tmp = path.with_extension("jsonl.prune-tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut out = std::fs::File::create(&tmp)?;
        out.write_all(&buf)?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp, path)
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
instruction itself; produce only the answer. Answer in the same language the session \
transcript is written in (if the transcript mixes languages, use its dominant \
language), even when no question is appended. A prior observer answer may be included \
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
    let mut sinks = ProgressSinks { content: &mut sink };
    let result = curl::call(&request, &mut sinks)
        .map_err(|e| io::Error::other(format!("summariser call failed: {e}")))?;
    pick_summary_text(&result).ok_or_else(|| io::Error::other("summariser returned empty content"))
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

/// Pick a usable summary from a [`CallResult`]: the assistant text.
/// Returns `None` for an empty (or whitespace-only) response so the
/// caller can surface a clear error rather than persisting a blank
/// summary.
fn pick_summary_text(result: &curl::CallResult) -> Option<String> {
    let content = result.content.trim();
    if content.is_empty() {
        return None;
    }
    Some(content.to_string())
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
            tool_calls,
        } => {
            content.len()
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
    // `safe_tail_start` returns 0 when the history is short (<= target)
    // or when no safe boundary exists in the initial tail. Starting at
    // 0 here means: if the *whole* history already fits the budget, we
    // skip (return None); if it is too large because of one huge record
    // even though there are few records, the loop below walks forward
    // to a safe boundary and folds it -- exactly the trigger hole this
    // guard closes.
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
    layers: &[(RuleScope, &[Rule])],
    authorization: &Authorization,
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
    counters: &mut Counters,
    dry_run: bool,
    timeout: Option<Duration>,
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
    let judgment = evaluate(layers, &inv.argv, authorization);
    let display = shell_escape_argv(&inv.argv);
    match judgment {
        Judgment::AutoApprove(dec) => {
            if !dry_run {
                let dec_display = shell_escape_argv(&dec.args_prefix);
                eprintln!(
                    "[command] auto-approve via {} rule '{}': {}",
                    dec.scope.as_str(),
                    dec_display,
                    display
                );
                append_auto_approval(session, &tc.id, ApprovalDecision::Approve, &dec)?;
                let content = match run_command_sync(&inv, executor, timeout) {
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
                let dec_display = shell_escape_argv(&dec.args_prefix);
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
                        "auto-denied by {} rule args_prefix {:?}",
                        dec.scope.as_str(),
                        dec.args_prefix
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
        args_prefix: dec.args_prefix.clone(),
        allow: dec.allowed,
        matches: dec
            .matches
            .iter()
            .map(|m| AutoDecidedMatch {
                scope: m.scope.as_str().to_string(),
                kind: m.kind.as_str().to_string(),
                allow: m.allow,
                args_prefix: m.args_prefix.clone(),
                path: m.path.clone(),
                adopted: m.adopted,
            })
            .collect(),
    };
    session.append(&SessionRecord::ToolApproval {
        ts: now_unix_millis(),
        call_id: call_id.to_string(),
        decision,
        auto_decided_by: Some(sidecar),
    })
}

/// Suggest the `attini approve --grant` invocation that would
/// pre-approve the argv-prefix of the pending command. `argv` is
/// truncated to at most two elements (typical pattern: `program
/// subcommand`) so the rule stays a general prefix rather than
/// baking every flag in.
fn emit_suggested_rule(argv: &[String]) {
    let Some(prefix) = grant_prefix(argv) else {
        return;
    };
    let prefix_display = shell_escape_argv(&prefix);
    eprintln!("suggested rule (fold into the next approve):");
    eprintln!("  attini approve --grant session      # allow {prefix_display} (session-local)");
    eprintln!("  attini approve --grant workspace    # allow {prefix_display} (workspace-wide)");
}

/// Truncate an argv to the prefix an auto-approve rule should use: at most
/// two elements (typical pattern: `program subcommand`) so the rule stays a
/// general prefix rather than baking every flag in. Shared by
/// [`emit_suggested_rule`] and `attini approve --grant` so the two can never
/// disagree about what prefix would be written.
fn grant_prefix(argv: &[String]) -> Option<Vec<String>> {
    if argv.is_empty() {
        return None;
    }
    let take = argv.len().min(2);
    Some(argv[..take].to_vec())
}

/// Resolved, not-yet-persisted grant for a single pending call. The
/// variant mirrors the pending tool kind: a command grant persists an
/// argv prefix, a read grant persists a canonical path.
#[derive(Debug)]
enum GrantIntent {
    Command(Vec<String>),
    Read(String),
}

/// Validate and resolve the `attini approve --grant` request against the
/// pending set, returning the grant to persist (if any).
///
/// A grant that cannot be formed is an error, not a silent no-op: if the
/// pending call(s) are not exactly one grantable call, or the prefix/path
/// cannot be derived, `--grant` is rejected before any approval is
/// recorded. `--grant oneshot` (and no grant at all) never persist
/// anything and therefore never depend on the pending set.
///
/// `attini approve` on a session with no pending call (it stopped at
/// `max_turns`) falls back to a plain continuation, in which case there is
/// nothing to grant: `--grant` is silently ignored there rather than
/// errored, since the human's intent was simply "keep going".
fn plan_grant(
    request: GrantRequest,
    pendings: &[Pending],
    workspace_root: &std::path::Path,
) -> io::Result<Option<GrantIntent>> {
    match request {
        GrantRequest::None | GrantRequest::Oneshot => return Ok(None),
        GrantRequest::Session | GrantRequest::Workspace => {}
    }
    let [pending] = pendings else {
        return Err(io::Error::other(
            "--grant is ambiguous with multiple pending calls; approve one at a time".to_string(),
        ));
    };
    match pending.tool_kind {
        PendingToolKind::Command => {
            let inv = CommandInvocation::parse(&pending.arguments_json).map_err(|e| {
                io::Error::other(format!(
                    "--grant: could not read the pending command: {e:?}"
                ))
            })?;
            match grant_prefix(&inv.argv) {
                Some(prefix) => Ok(Some(GrantIntent::Command(prefix))),
                None => Err(io::Error::other(
                    "--grant: the pending command has no argv to persist".to_string(),
                )),
            }
        }
        PendingToolKind::Read => {
            // Persist the canonical target path, matching the one-shot
            // root the read is executed with, so a later grant-based load
            // resolves the same directory.
            let inv = ReadOnlyTool::parse(&pending.function_name, &pending.arguments_json)
                .map_err(|e| {
                    io::Error::other(format!("--grant: could not read the pending read: {e:?}"))
                })?;
            match read_extra_root(&inv, workspace_root) {
                // Persist an in-workspace target as a workspace-relative
                // path, the same shape the `read` deny check evaluates
                // (`workspace_relative_read_target`), so a grant and a
                // deny rule compare like-for-like under last-match-wins.
                // Out-of-workspace targets keep the absolute canonical
                // path, which the executor accepts as a root.
                Some(path) => Ok(Some(GrantIntent::Read(grant_read_path(
                    &path,
                    workspace_root,
                )))),
                None => Err(io::Error::other(
                    "--grant: the pending read has no resolvable path to persist".to_string(),
                )),
            }
        }
        PendingToolKind::Patch => Err(io::Error::other(
            "--grant applies to commands and reads only; there is no scope for a patch".to_string(),
        )),
    }
}

/// Best-effort persistence of the resolved grant, run after every pending
/// call has been approved. The approval already stands, so any failure is
/// reported as a one-line warning rather than rolling the approval back.
fn apply_grant(cfg: &TellConfig, intent: &GrantIntent) {
    let scope = match cfg.grant_request {
        GrantRequest::Session => permissions::GrantScope::Session(&cfg.session_name),
        GrantRequest::Workspace => permissions::GrantScope::Workspace,
        GrantRequest::None | GrantRequest::Oneshot => return,
    };
    let outcome = match intent {
        GrantIntent::Command(argv_prefix) => permissions::grant(scope, argv_prefix),
        GrantIntent::Read(path) => permissions::grant_read(scope, path),
    };
    let display = match intent {
        GrantIntent::Command(argv_prefix) => shell_escape_argv(argv_prefix),
        GrantIntent::Read(path) => path.clone(),
    };
    match outcome {
        Ok(permissions::GrantOutcome::Appended(path)) => {
            eprintln!(
                "[approve] granted: appended '{display}' to {}",
                path.display()
            );
        }
        Ok(permissions::GrantOutcome::AlreadyGranted(path)) => {
            eprintln!("[approve] already granted (no-op): {}", path.display());
        }
        Err(e) => {
            eprintln!("[approve] warning: grant of '{display}' failed: {e}; approval still stands");
        }
    }
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

/// Outcome of handling one read-only tool call.
enum ReadOnlyDispatch {
    /// Answered inline; `errored` is `true` iff `content` is a
    /// `tool_error_json` payload (parse failure or executor error) so the
    /// caller can update `Counters::tool_errors` without re-parsing.
    Done {
        summary: String,
        content: String,
        errored: bool,
    },
    /// The call targeted a path outside the workspace; park it for human
    /// approval instead of answering with an error. Carries the display
    /// line shown to the human and the preview stored in `pending.json`.
    NeedsApproval { summary: String, preview: String },
}

/// Returns the dispatch result for one read-only tool call. `errored`
/// in the `Done` case is `true` iff the returned content is a
/// `tool_error_json` payload.
///
/// `permission_layers` is consulted for a workspace-internal `read`
/// `allow:false` rule (case 2): if the winning rule denies the target,
/// the call is parked for a one-shot approval instead of being answered
/// with a silent error. The deny check runs before the executor so a
/// deny rule short-circuits even though the path is inside a root the
/// executor would otherwise accept.
fn run_read_only(
    tc: &ToolCall,
    executor: &ToolExecutor,
    permission_layers: &[ScopedRules<'_>],
    authorization: &Authorization,
) -> ReadOnlyDispatch {
    match ReadOnlyTool::parse(&tc.function_name, &tc.arguments_json) {
        Ok(inv) => {
            let args_summary = summarize_read_only(&inv);
            // Case 2: a `read` `allow:false` rule that wins the override
            // chain turns the read into an approval request, even when the
            // path is inside the workspace.
            if let Some(rel) = workspace_relative_read_target(&inv, executor.root())
                && let Judgment::AutoDeny(_) =
                    evaluate_read(permission_layers, Path::new(&rel), authorization)
            {
                let preview = format!(r#"{args_summary} (denied by a read rule)"#);
                return ReadOnlyDispatch::NeedsApproval {
                    summary: format!("[{args_summary}] approval required"),
                    preview,
                };
            }
            match executor.execute(inv.clone()) {
                ToolOutcome::Ok(payload) => {
                    let mut summary = format!("[{args_summary}] ok");
                    // Echo the head of a `read` result to stderr so a
                    // human can see what was read without opening the
                    // file. Display only: the payload sent to the model
                    // is unchanged.
                    if let Some(preview) = read_content_preview(&payload) {
                        summary.push('\n');
                        summary.push_str(&preview);
                    }
                    ReadOnlyDispatch::Done {
                        summary,
                        content: payload,
                        errored: false,
                    }
                }
                // Outside the workspace: this is the one read error we can
                // turn into an approval request. Both the workspace and any
                // granted roots were tried; offer the human the chance to
                // widen the boundary for this single call. `inv` is reused
                // (cloned) by `execute_pending` on approval.
                ToolOutcome::Err(ToolExecutionError::OutsideWorkspace) => {
                    let preview = format!(r#"{args_summary} (outside workspace)"#);
                    ReadOnlyDispatch::NeedsApproval {
                        summary: format!("[{args_summary}] approval required"),
                        preview,
                    }
                }
                ToolOutcome::Err(err) => ReadOnlyDispatch::Done {
                    summary: format!("[{args_summary}] err: {}", short_err(&err)),
                    content: tool_error_json_from(&err),
                    errored: true,
                },
            }
        }
        Err(err) => ReadOnlyDispatch::Done {
            summary: format!("[{}] parse err: {}", tc.function_name, short_err(&err)),
            content: tool_error_json_from(&err),
            errored: true,
        },
    }
}

/// Path a read-only invocation targets, for the purpose of deriving a
/// one-shot extra read root after approval. `read`/`list` use their
/// `path`; `search` uses its `path_prefix` when present (`None` means
/// the workspace root, which never needs approval).
fn read_only_target(inv: &ReadOnlyTool) -> Option<&str> {
    match inv {
        ReadOnlyTool::List { path, .. } => Some(path),
        ReadOnlyTool::Read { path, .. } => Some(path),
        ReadOnlyTool::Search { path_prefix, .. } => path_prefix.as_deref(),
    }
}

/// Resolve a read-only call's target to a workspace-relative path (with
/// `/`-separated components preserved as written) for the purpose of
/// matching `read` rules. Returns `None` when there is no target, the
/// target resolves outside the workspace, or it cannot be canonicalised.
///
/// Rule paths are stored as the human wrote them (usually
/// workspace-relative, e.g. `src/secret`), so an in-workspace target is
/// reduced to that same shape before matching.
fn workspace_relative_read_target(inv: &ReadOnlyTool, workspace_root: &Path) -> Option<String> {
    let target = read_only_target(inv)?;
    let candidate = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace_root.join(target)
    };
    let canon = candidate.canonicalize().ok()?;
    let root = workspace_root.canonicalize().ok()?;
    let rel = canon.strip_prefix(&root).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

/// The path to persist in a `read` allow rule for a granted read.
///
/// Inside the workspace, the rule is stored workspace-relative (e.g.
/// `secret_dir/secret.txt`) so it matches the same form the `read` deny
/// check evaluates — otherwise an absolute allow rule and a relative
/// deny rule would not compare under last-match-wins. Outside the
/// workspace, the absolute canonical path is kept, since the executor
/// accepts it as a root and there is no workspace-relative form.
fn grant_read_path(canonical: &Path, workspace_root: &Path) -> String {
    let root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    match canonical.strip_prefix(&root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => canonical.display().to_string(),
    }
}

/// Resolve the approved read target to an absolute canonical path to be
/// used as a one-shot extra read root. Relative targets resolve against
/// `workspace_root`, matching [`resolve_within_any`]'s semantics. Returns
/// `None` when there is no target or it does not exist on disk.
fn read_extra_root(inv: &ReadOnlyTool, workspace_root: &Path) -> Option<PathBuf> {
    let target = read_only_target(inv)?;
    let candidate = if std::path::Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace_root.join(target)
    };
    candidate.canonicalize().ok()
}

/// Aggregate verdict of the `write` rules over every edit in a patch.
enum WriteVerdict {
    /// Every edit is allowed by a winning `write` `allow:true` rule.
    Allowed,
    /// At least one edit is denied by a winning `write` `allow:false`
    /// rule. Denial wins over anything else.
    Denied,
    /// No edit is denied, but at least one is not covered by an
    /// allow rule, so fall back to the git-tracking heuristic.
    Undecided,
}

/// Evaluate every edit in `inv` against the `write` rules. A single
/// denied edit denies the whole patch; otherwise the patch is allowed
/// only when every edit is covered by a winning `allow:true` rule.
fn patch_write_verdict(
    inv: &PatchInvocation,
    permission_layers: &[(RuleScope, &[Rule])],
    authorization: &Authorization,
    workspace_root: &Path,
) -> WriteVerdict {
    let mut all_allowed = true;
    for edit in &inv.edits {
        let Some(rel) = workspace_relative_write_target(edit.path(), workspace_root) else {
            // A path we cannot reduce to a workspace-relative form (an
            // absolute path the executor would reject anyway, or one
            // that does not canonicalise) cannot be covered by a
            // workspace-relative `write` rule.
            all_allowed = false;
            continue;
        };
        match evaluate_write(permission_layers, Path::new(&rel), authorization) {
            Judgment::AutoDeny(_) => return WriteVerdict::Denied,
            Judgment::AutoApprove(_) => {}
            Judgment::Pending => all_allowed = false,
        }
    }
    if all_allowed {
        WriteVerdict::Allowed
    } else {
        WriteVerdict::Undecided
    }
}

/// Reduce a patch edit's target path to the workspace-relative form a
/// `write` rule is compared against. Returns `None` when the path
/// escapes the workspace or cannot be canonicalised.
fn workspace_relative_write_target(target: &str, workspace_root: &Path) -> Option<String> {
    let candidate = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace_root.join(target)
    };
    let canon = candidate.canonicalize().ok()?;
    let root = workspace_root.canonicalize().ok()?;
    let rel = canon.strip_prefix(&root).ok()?;
    Some(rel.to_string_lossy().into_owned())
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

/// Render the head of a `read` result's `content` for stderr,
/// capped at [`READ_PREVIEW_MAX_LINES`] lines with a
/// `... (N more lines omitted)` marker. Returns `None` when the
/// payload is not a readable object with a string `content` member
/// (e.g. a `list` or `search` result), so callers can append it
/// unconditionally. Never fails: display is best-effort.
fn read_content_preview(payload: &str) -> Option<String> {
    let json = RawJson::parse(payload).ok()?;
    let content = json
        .value()
        .to_member("content")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .ok()?;
    let mut out = String::new();
    let mut shown: usize = 0;
    let mut omitted: usize = 0;
    for line in content.lines() {
        if shown < READ_PREVIEW_MAX_LINES {
            out.push_str("  | ");
            out.push_str(line);
            out.push('\n');
            shown += 1;
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        out.push_str(&format!("  | ... ({omitted} more lines omitted)\n"));
    }
    // Trim the trailing newline so the caller controls spacing.
    while out.ends_with('\n') {
        out.pop();
    }
    if out.is_empty() { None } else { Some(out) }
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
#[allow(clippy::too_many_arguments)]
fn dispatch_patch_unapproved(
    tc: &ToolCall,
    executor: &ToolExecutor,
    permission_layers: &[(RuleScope, &[Rule])],
    authorization: &Authorization,
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
    let (preview_content, preview) = match executor.preview_patch(&inv) {
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
    // A patch auto-runs when either every edit is a git-tracked Update
    // (revertible, no prompt needed) or every edit is permitted by a
    // `write` `allow:true` rule. Any edit whose winning `write` rule is
    // `allow:false` forces approval, regardless of git tracking.
    let write_verdict =
        patch_write_verdict(&inv, permission_layers, authorization, executor.root());
    let auto_approve = match write_verdict {
        WriteVerdict::Denied => false,
        WriteVerdict::Allowed => true,
        WriteVerdict::Undecided => preview.auto_approve,
    };
    if auto_approve {
        if !dry_run {
            match executor.apply_patch(&inv, &preview_content) {
                Ok(paths) => {
                    eprintln!(
                        "[patch] auto-approved: {} file(s) (git-tracked)",
                        paths.len()
                    );
                    eprintln!("{}", render_patch_diff(&inv));
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
        let preview_text = render_patch_preview_text(&preview, &inv);
        eprintln!("[patch] approval required");
        eprintln!("{preview_text}");
        // Re-state the approval request after the (possibly long) diff so the
        // decision prompt lands at the bottom of the terminal, next to the
        // summary the human needs, instead of being pushed off-screen by the
        // diff body.
        eprintln!("{}", render_patch_approval_footer(&preview));
        Ok(PatchDispatch::Awaiting(build_pending(
            tc,
            PendingToolKind::Patch,
            preview_text,
        )))
    }
}

/// One-line approval restatement shown after the diff body, so the human can
/// decide without scrolling back up past the diff.
fn render_patch_approval_footer(p: &PatchPreview) -> String {
    format!(
        "[patch] approval required: {} edit(s) across {} file(s), +{} / -{} lines",
        p.edit_count,
        p.target_paths.len(),
        p.added_lines,
        p.removed_lines
    )
}

fn render_patch_preview_text(p: &PatchPreview, inv: &PatchInvocation) -> String {
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
    out.push('\n');
    out.push_str(&render_patch_diff(inv));
    out
}

/// Render a readable per-edit diff body (no hunk headers): each
/// `before` line as `- ...` and each `after` line as `+ ...`, with
/// `Add` content all `+`. Output is capped at
/// [`PATCH_PREVIEW_MAX_LINES`] lines so a pathological patch cannot
/// flood the terminal; the overflow is summarised as
/// `... (N more lines omitted)`.
fn render_patch_diff(inv: &PatchInvocation) -> String {
    let mut out = String::new();
    let mut shown: usize = 0;
    let mut omitted: usize = 0;
    // A line counts toward the cap if it carries a `-`/`+`/header;
    // this keeps the accounting honest even across many edits.
    let push = |out: &mut String, shown: &mut usize, omitted: &mut usize, line: String| {
        if *shown < PATCH_PREVIEW_MAX_LINES {
            out.push_str(&line);
            out.push('\n');
            *shown += 1;
        } else {
            *omitted += 1;
        }
    };
    for edit in &inv.edits {
        match edit {
            PatchTool::Add { path, content } => {
                push(&mut out, &mut shown, &mut omitted, format!("  add {path}"));
                for line in content.lines() {
                    push(&mut out, &mut shown, &mut omitted, format!("    + {line}"));
                }
            }
            PatchTool::Update {
                path,
                before,
                after,
            } => {
                push(
                    &mut out,
                    &mut shown,
                    &mut omitted,
                    format!("  update {path}"),
                );
                for line in before.lines() {
                    push(&mut out, &mut shown, &mut omitted, format!("    - {line}"));
                }
                for line in after.lines() {
                    push(&mut out, &mut shown, &mut omitted, format!("    + {line}"));
                }
            }
        }
    }
    if omitted > 0 {
        out.push_str(&format!("    ... ({omitted} more lines omitted)\n"));
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

fn execute_pending(
    pending: &Pending,
    executor: &ToolExecutor,
    timeout: Option<Duration>,
) -> io::Result<String> {
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
            let (preview_content, _preview) = match executor.preview_patch(&inv) {
                Ok(x) => x,
                Err(e) => return Ok(tool_error_json("patch_preview", &format!("{e:?}"))),
            };
            match executor.apply_patch(&inv, &preview_content) {
                Ok(paths) => Ok(patch_result_json(&paths)),
                Err(e) => Ok(tool_error_json("patch_apply", &format!("{e:?}"))),
            }
        }
        PendingToolKind::Command => {
            let inv = CommandInvocation::parse(&pending.arguments_json)
                .map_err(|e| io::Error::other(format!("command args: {e:?}")))?;
            match run_command_sync(&inv, executor, timeout) {
                Ok(s) => Ok(s),
                Err(err) => {
                    let (code, msg) = err.to_code_and_message();
                    Ok(tool_error_json(code, &msg))
                }
            }
        }
        PendingToolKind::Read => {
            // A read approved to reach outside the workspace executes with
            // a one-shot extra root derived from the requested path, so the
            // boundary widens for exactly this call and is never persisted.
            let inv = match ReadOnlyTool::parse(&pending.function_name, &pending.arguments_json) {
                Ok(inv) => inv,
                Err(e) => return Ok(tool_error_json_from(&e)),
            };
            let Some(extra) = read_extra_root(&inv, executor.root()) else {
                return Ok(tool_error_json(
                    "read_args",
                    "approved read has no resolvable path",
                ));
            };
            match executor.execute_with_extra_read_root(inv, extra) {
                ToolOutcome::Ok(payload) => Ok(payload),
                ToolOutcome::Err(e) => Ok(tool_error_json_from(&e)),
            }
        }
    }
}

/// Resolve the configured `command` timeout into a [`Duration`], or
/// `None` when the cap is disabled (`--command-timeout 0`).
fn command_timeout(cfg: &TellConfig) -> Option<Duration> {
    match cfg.command_timeout_seconds {
        Some(0) | None => None,
        Some(secs) => Some(Duration::from_secs(secs)),
    }
}

fn run_command_sync(
    inv: &CommandInvocation,
    executor: &ToolExecutor,
    timeout: Option<Duration>,
) -> Result<String, CommandError> {
    let started = Instant::now();
    // argv is guaranteed non-empty by CommandInvocation::parse.
    let mut cmd = Command::new(&inv.argv[0]);
    cmd.args(&inv.argv[1..]).current_dir(executor.root());
    let output = crate::child_output::run_streamed(&mut cmd, timeout).map_err(|e| {
        CommandError::SpawnFailed {
            message: e.to_string(),
        }
    })?;
    let elapsed = started.elapsed();
    let termination_reason = if output.timed_out {
        "timeout"
    } else if output.status.code().is_some() {
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
        output.truncated,
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
/// is cleared so a later `--approve` does not double-run
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
            // prompt (bypassing --approve), drop the pending so
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
                    args_prefix: Vec::new(),
                    allow: false,
                    matches: Vec::new(),
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
        .ok_or_else(|| io::Error::other("no pending.json — nothing to approve"))
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
    truncated: bool,
) -> String {
    struct Payload<'a> {
        stdout: &'a str,
        stderr: &'a str,
        exit_code: Option<i32>,
        termination_reason: &'a str,
        duration_ms: u64,
        truncated: bool,
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
                f.member("truncated", self.truncated)?;
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
        truncated,
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
    fn compaction_cutoff_folds_few_but_huge_records() {
        // The trigger hole: very few records but one enormous tool
        // result dominates the history. `latest_prompt_tokens` would
        // be stale/small, so the size-based guard must fire even when
        // `n <= target_keep`. The cutoff should walk past the huge
        // record and fold it (returning `Some(n)` when no safe
        // boundary is reachable, or a boundary that fits the budget).
        let records = vec![user(1), assistant_with_tool_call(2), big_tool(3, 300_000)];
        assert_eq!(compaction_cutoff(&records, 10, 1000), Some(records.len()));
    }

    #[test]
    fn compaction_cutoff_skips_when_few_and_small() {
        // Few records that all fit the budget: nothing to fold.
        let records = vec![user(1), assistant_plain(2)];
        assert_eq!(compaction_cutoff(&records, 10, 1000), None);
    }

    #[test]
    fn should_auto_compact_fires_on_token_threshold() {
        // Last successful turn was large enough: compact on the token
        // threshold alone, even if the recorded size is tiny.
        assert!(should_auto_compact(COMPACTION_TRIGGER_TOKENS, 0));
        assert!(should_auto_compact(COMPACTION_TRIGGER_TOKENS + 1, 100));
    }

    #[test]
    fn should_auto_compact_fires_on_record_size_when_tokens_stale() {
        // The trigger hole: the token count is stale/small because the
        // previous turn suspended, but the real records are huge. The
        // size guard must fire even below the token threshold.
        assert!(should_auto_compact(0, RECORDS_TOTAL_MAX_CHARS + 1));
        assert!(should_auto_compact(1000, RECORDS_TOTAL_MAX_CHARS + 1));
    }

    #[test]
    fn should_auto_compact_stays_quiet_when_everything_is_small() {
        // Both signals below their thresholds: no compaction.
        assert!(!should_auto_compact(0, RECORDS_TOTAL_MAX_CHARS));
        assert!(!should_auto_compact(COMPACTION_TRIGGER_TOKENS - 1, 10));
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
    // Automatic pruning
    // -------------------------------------------------------------

    fn prune_tempdir(name: &str) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("attini-prune-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create tempdir");
        base
    }

    fn write_lines(path: &std::path::Path, lines: &[&str]) {
        use std::io::Write as _;
        let mut f = std::fs::File::create(path).expect("create");
        for l in lines {
            f.write_all(l.as_bytes()).expect("write");
            f.write_all(b"\n").expect("newline");
        }
        f.sync_all().expect("sync");
    }

    #[test]
    fn line_is_safe_boundary_classifies_records_as_expected() {
        assert!(line_is_safe_boundary(
            r#"{"kind":"user","ts":1,"text":"hi"}"#
        ));
        assert!(line_is_safe_boundary(
            r#"{"kind":"assistant","ts":2,"text":"done","tool_calls":[]}"#
        ));
        // Assistant with empty text but no tool calls is a final answer.
        assert!(line_is_safe_boundary(
            r#"{"kind":"assistant","ts":3,"text":"","tool_calls":[]}"#
        ));
        // Assistant awaiting tools is unsafe.
        assert!(!line_is_safe_boundary(
            r#"{"kind":"assistant","ts":4,"text":"","tool_calls":[{"id":"c"}]}"#
        ));
        assert!(!line_is_safe_boundary(
            r#"{"kind":"tool","ts":5,"text":"x"}"#
        ));
        assert!(!line_is_safe_boundary(r#"{"kind":"summary","ts":6}"#));
        assert!(!line_is_safe_boundary("not json"));
        assert!(!line_is_safe_boundary(""));
    }

    #[test]
    fn prune_offset_past_midpoint_lands_on_safe_boundary_after_half() {
        let dir = prune_tempdir("midpoint");
        let path = dir.join("conv.jsonl");
        // Pad the file so the midpoint falls inside the first chunks.
        let pad = "x".repeat(400);
        let lines = [
            format!(r#"{{"kind":"user","ts":1,"text":"{pad}"}}"#),
            format!(r#"{{"kind":"tool","ts":2,"text":"{pad}"}}"#),
            format!(r#"{{"kind":"assistant","ts":3,"text":"{pad}","tool_calls":[]}}"#),
            format!(r#"{{"kind":"user","ts":4,"text":"{pad}"}}"#),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_lines(&path, &refs);
        let size = std::fs::metadata(&path).expect("meta").len();
        let (offset, dropped) = prune_offset_past_midpoint(&path, size)
            .expect("ok")
            .expect("boundary");
        // The chosen line must be a safe boundary at/after the midpoint.
        assert!(offset >= size / 2);
        assert!(dropped >= 1);
        // Compute the start offset of the first safe line at/after the
        // midpoint by replaying the line lengths.
        let mut cursor: u64 = 0;
        let mut expected: Option<(u64, u64)> = None;
        for (i, l) in refs.iter().enumerate() {
            let start = cursor;
            cursor += l.len() as u64 + 1;
            if start >= size / 2 && line_is_safe_boundary(l) {
                expected = Some((start, i as u64));
                break;
            }
        }
        assert_eq!(Some((offset, dropped)), expected);
    }

    #[test]
    fn prune_offset_past_midpoint_returns_none_when_pair_spans_tail() {
        let dir = prune_tempdir("no_safe");
        let path = dir.join("conv.jsonl");
        let pad = "x".repeat(400);
        // Past the midpoint the file is a single unresolved
        // assistant -> tool pair with no safe boundary, so no cut.
        let lines = [
            format!(r#"{{"kind":"user","ts":1,"text":"{pad}"}}"#),
            r#"{"kind":"assistant","ts":2,"text":"","tool_calls":[{"id":"c"}]}"#.to_string(),
            format!(r#"{{"kind":"tool","ts":3,"text":"{pad}"}}"#),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_lines(&path, &refs);
        let size = std::fs::metadata(&path).expect("meta").len();
        assert!(
            prune_offset_past_midpoint(&path, size)
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn rewrite_file_from_offset_keeps_suffix_only() {
        let dir = prune_tempdir("rewrite");
        let path = dir.join("conv.jsonl");
        let lines = [
            r#"{"kind":"user","ts":1,"text":"first"}"#,
            r#"{"kind":"summary","ts":2,"text":"s1"}"#,
            r#"{"kind":"user","ts":3,"text":"after"}"#,
        ];
        write_lines(&path, &lines);
        let offset: u64 = (lines[0].len() + 1 + lines[1].len() + 1) as u64;
        rewrite_file_from_offset(&path, offset).expect("rewrite ok");
        let contents = std::fs::read_to_string(&path).expect("read");
        let kept: Vec<&str> = contents.trim_end_matches('\n').split('\n').collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].contains("after"));
    }

    // -------------------------------------------------------------
    // ToolCallGate
    // -------------------------------------------------------------

    fn gate_config(
        turn_limit: usize,
        rate: Option<RateLimit>,
        session_max: Option<usize>,
    ) -> TellConfig {
        TellConfig {
            session_name: String::new(),
            model: String::new(),
            max_tokens: None,
            workspace_root: PathBuf::new(),
            system_prompt: None,
            max_turns: 0,
            turn_tool_call_limit: turn_limit,
            tool_call_rate: rate,
            session_tool_call_max: session_max,
            authorization: Authorization::PerTool,
            temperature: None,
            grant_request: GrantRequest::None,
            command_timeout_seconds: None,
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
    // pick_summary_text
    // -------------------------------------------------------------

    fn call_result(content: &str) -> curl::CallResult {
        curl::CallResult {
            content: content.to_string(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }
    }

    #[test]
    fn pick_summary_text_returns_content_when_present() {
        let r = call_result("hello summary");
        assert_eq!(pick_summary_text(&r), Some("hello summary".to_string()));
    }

    #[test]
    fn pick_summary_text_returns_none_when_content_is_blank() {
        let r = call_result("   ");
        assert_eq!(pick_summary_text(&r), None);
    }

    // -----------------------------------------------------------------
    // abbreviate_args / render_records_prose
    // -----------------------------------------------------------------

    #[test]
    fn abbreviate_args_strips_newlines_and_truncates() {
        let long = "{\"path\":\"src/tell_cli.rs\",\n\"pattern\":\"".repeat(40);
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
            false,
        );
        assert!(json.contains("\"stdout\":\"line 1\\nline 2\\n"));
        assert!(json.contains("\"stderr\":\"warning: something\\n"));
        assert!(json.contains("\"exit_code\":1"));
        assert!(json.contains("\"termination_reason\":\"exited\""));
        assert!(json.contains("\"duration_ms\":123"));
        assert!(json.contains("\"truncated\":false"));
    }

    #[test]
    fn command_result_json_roundtrips_no_exit_code() {
        let json = command_result_json("out", "", None, "signaled", Duration::ZERO, true);
        assert!(json.contains("\"exit_code\":null"));
        assert!(json.contains("\"termination_reason\":\"signaled\""));
        assert!(json.contains("\"truncated\":true"));
    }

    #[test]
    fn command_result_json_includes_truncated_flag() {
        let json = command_result_json("out", "", Some(0), "exited", Duration::ZERO, true);
        assert!(json.contains("\"truncated\":true"));
        assert!(!json.contains("\"truncated\":false"));
    }

    // -----------------------------------------------------------------
    // command_timeout
    // -----------------------------------------------------------------

    #[test]
    fn command_timeout_zero_and_none_disable_the_cap() {
        let mut cfg = gate_config(1, None, None);
        cfg.command_timeout_seconds = None;
        assert_eq!(command_timeout(&cfg), None);
        cfg.command_timeout_seconds = Some(0);
        assert_eq!(command_timeout(&cfg), None);
    }

    #[test]
    fn command_timeout_seconds_becomes_a_duration() {
        let mut cfg = gate_config(1, None, None);
        cfg.command_timeout_seconds = Some(180);
        assert_eq!(command_timeout(&cfg), Some(Duration::from_secs(180)));
    }

    // -------------------------------------------------------------
    // repair_messages (transcript integrity)
    // -------------------------------------------------------------

    fn assistant_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage::Assistant {
            content: String::new(),
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
    // render_tell_status_line
    // -------------------------------------------------------------

    #[test]
    fn status_line_render_includes_session_and_ctx() {
        let line = render_tell_status_line("deepseek-v4-flash", "main", 20736);
        assert_eq!(
            line,
            "[tell] model=deepseek-v4-flash session=main ctx=20736"
        );
    }

    #[test]
    fn status_line_uses_passed_ctx_as_current_size() {
        let line = render_tell_status_line("m", "s", 1000);
        assert!(line.contains("ctx=1000"));
        assert!(line.contains("model=m"));
        assert!(line.contains("session=s"));
    }

    // -------------------------------------------------------------
    // max_turns_error / RESUME_PROMPT
    // -------------------------------------------------------------

    #[test]
    fn max_turns_error_points_at_approve_and_tell() {
        let msg = max_turns_error(20);
        assert!(msg.contains("max_turns=20"));
        // The continuation command sits on its own line, ready to copy,
        // and does not restate the (implicit) session name.
        assert!(msg.contains("\nattini approve  # or give a new instruction"));
        assert!(!msg.contains("-s "));
    }

    #[test]
    fn resume_prompt_is_non_empty() {
        assert!(!RESUME_PROMPT.trim().is_empty());
    }

    // -------------------------------------------------------------
    // normalise_pending_free_approve
    // -------------------------------------------------------------

    #[test]
    fn pending_free_approve_retries_a_transport_error() {
        let cont = normalise_pending_free_approve(Some(InvocationEndReason::TransportError));
        assert!(matches!(cont, Continuation::Retry));
    }

    #[test]
    fn pending_free_approve_continues_after_other_endings() {
        for last in [
            Some(InvocationEndReason::Completed),
            Some(InvocationEndReason::AwaitingApproval),
            Some(InvocationEndReason::Error),
            Some(InvocationEndReason::SessionToolCallExhausted),
            None,
        ] {
            let cont = normalise_pending_free_approve(last);
            match cont {
                Continuation::Prompt(text) => assert_eq!(text, RESUME_PROMPT),
                Continuation::Retry => panic!("expected Prompt for {last:?}, got Retry"),
                Continuation::Approve => {
                    panic!("expected Prompt for {last:?}, got Approve")
                }
            }
        }
    }

    // -------------------------------------------------------------
    // render_tool_batching_note
    // -------------------------------------------------------------

    #[test]
    fn tool_batching_note_forbids_read_only_after_approval_gated_call() {
        let note = render_tool_batching_note();
        assert!(note.contains("Tool call batching"));
        assert!(note.contains("approval"));
        assert!(note.contains("last"));
        assert!(note.contains("read"));
        assert!(note.contains("search"));
        assert!(note.contains("do not put"));
    }

    // -------------------------------------------------------------
    // grant_prefix / plan_grant
    // -------------------------------------------------------------

    fn command_pending(call_id: &str, argv: &[&str]) -> Pending {
        let argv_json = argv
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(",");
        Pending {
            ts: 0,
            call_id: call_id.to_string(),
            tool_kind: PendingToolKind::Command,
            function_name: "command".to_string(),
            arguments_json: format!("{{\"argv\":[{argv_json}]}}"),
            preview: String::new(),
        }
    }

    #[test]
    fn grant_prefix_truncates_to_two_elements() {
        assert_eq!(grant_prefix(&[]), None);
        assert_eq!(
            grant_prefix(&["cargo".to_string()]),
            Some(vec!["cargo".to_string()])
        );
        assert_eq!(
            grant_prefix(&["cargo".to_string(), "test".to_string(), "-q".to_string()]),
            Some(vec!["cargo".to_string(), "test".to_string()])
        );
    }

    fn read_pending(call_id: &str, path: &str) -> Pending {
        Pending {
            ts: 0,
            call_id: call_id.to_string(),
            tool_kind: PendingToolKind::Read,
            function_name: "read".to_string(),
            arguments_json: format!("{{\"path\":\"{path}\"}}"),
            preview: String::new(),
        }
    }

    #[test]
    fn plan_grant_none_and_oneshot_never_persist() {
        let root = std::env::temp_dir();
        let pendings = vec![command_pending("c1", &["cargo", "test"])];
        assert!(
            plan_grant(GrantRequest::None, &pendings, &root)
                .unwrap()
                .is_none()
        );
        assert!(
            plan_grant(GrantRequest::Oneshot, &pendings, &root)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn plan_grant_session_resolves_command_prefix() {
        let root = std::env::temp_dir();
        let pendings = vec![command_pending("c1", &["cargo", "test", "--all"])];
        match plan_grant(GrantRequest::Session, &pendings, &root).unwrap() {
            Some(GrantIntent::Command(prefix)) => {
                assert_eq!(prefix, vec!["cargo".to_string(), "test".to_string()]);
            }
            other => panic!("expected a command grant, got {other:?}"),
        }
    }

    #[test]
    fn plan_grant_rejects_patch_pending() {
        let root = std::env::temp_dir();
        let pendings = vec![Pending {
            ts: 0,
            call_id: "p1".to_string(),
            tool_kind: PendingToolKind::Patch,
            function_name: "patch".to_string(),
            arguments_json: "{}".to_string(),
            preview: String::new(),
        }];
        let err = plan_grant(GrantRequest::Session, &pendings, &root).unwrap_err();
        assert!(err.to_string().contains("commands and reads only"), "{err}");
    }

    #[test]
    fn plan_grant_rejects_multiple_pending_commands() {
        let root = std::env::temp_dir();
        let pendings = vec![
            command_pending("c1", &["cargo", "test"]),
            command_pending("c2", &["git", "status"]),
        ];
        let err = plan_grant(GrantRequest::Workspace, &pendings, &root).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn plan_grant_session_resolves_read_path() {
        let dir = std::env::temp_dir().join(format!("attini-plan-grant-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("outside.txt");
        std::fs::write(&file, "hi").expect("write");
        // A workspace root elsewhere makes the absolute target "outside".
        let root = std::env::temp_dir().join("attini-plan-grant-other");
        let pendings = vec![read_pending("r1", &file.display().to_string())];
        match plan_grant(GrantRequest::Session, &pendings, &root).unwrap() {
            Some(GrantIntent::Read(path)) => {
                assert_eq!(path, file.canonicalize().unwrap().display().to_string());
            }
            other => panic!("expected a read grant, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn plan_grant_rejects_unresolvable_read() {
        let root = std::env::temp_dir();
        let pendings = vec![read_pending("r1", "no-such-file-xyz.txt")];
        let err = plan_grant(GrantRequest::Session, &pendings, &root).unwrap_err();
        assert!(err.to_string().contains("no resolvable path"), "{err}");
    }

    // -------------------------------------------------------------
    // render_patch_diff / render_patch_preview_text
    // -------------------------------------------------------------

    fn patch_inv(edits: Vec<PatchTool>) -> PatchInvocation {
        PatchInvocation { edits }
    }

    #[test]
    fn patch_diff_shows_before_and_after_lines() {
        let inv = patch_inv(vec![PatchTool::Update {
            path: "src/a.rs".to_string(),
            before: "let x = 1;\nlet y = 2;".to_string(),
            after: "let x = 1;\nlet y = 3;".to_string(),
        }]);
        let out = render_patch_diff(&inv);
        assert!(out.contains("  update src/a.rs"), "{out}");
        assert!(out.contains("    - let y = 2;"), "{out}");
        assert!(out.contains("    + let y = 3;"), "{out}");
    }

    #[test]
    fn patch_diff_shows_add_content() {
        let inv = patch_inv(vec![PatchTool::Add {
            path: "src/new.rs".to_string(),
            content: "fn main() {}".to_string(),
        }]);
        let out = render_patch_diff(&inv);
        assert!(out.contains("  add src/new.rs"), "{out}");
        assert!(out.contains("    + fn main() {}"), "{out}");
    }

    #[test]
    fn patch_diff_caps_output_and_notes_omissions() {
        let many: String = (0..(PATCH_PREVIEW_MAX_LINES + 50))
            .map(|i| format!("line {i}\n"))
            .collect();
        let inv = patch_inv(vec![PatchTool::Add {
            path: "big.txt".to_string(),
            content: many,
        }]);
        let out = render_patch_diff(&inv);
        assert!(out.contains("more lines omitted"), "{out}");
        // The cap applies to body lines; total printed lines are bounded.
        assert!(
            out.lines().count() <= PATCH_PREVIEW_MAX_LINES + 1,
            "printed {} lines",
            out.lines().count()
        );
    }

    #[test]
    fn patch_preview_text_includes_diff() {
        let inv = patch_inv(vec![PatchTool::Update {
            path: "src/a.rs".to_string(),
            before: "old".to_string(),
            after: "new".to_string(),
        }]);
        let preview = PatchPreview {
            target_paths: vec!["src/a.rs".to_string()],
            added_lines: 1,
            removed_lines: 1,
            edit_count: 1,
            auto_approve: true,
        };
        let out = render_patch_preview_text(&preview, &inv);
        assert!(out.contains("patch preview: 1 edit(s)"), "{out}");
        assert!(out.contains("    - old"), "{out}");
        assert!(out.contains("    + new"), "{out}");
    }

    #[test]
    fn patch_approval_footer_restates_summary() {
        let preview = PatchPreview {
            target_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            added_lines: 3,
            removed_lines: 1,
            edit_count: 2,
            auto_approve: false,
        };
        let plain = render_patch_approval_footer(&preview);
        assert_eq!(
            plain,
            "[patch] approval required: 2 edit(s) across 2 file(s), +3 / -1 lines"
        );
    }

    // -------------------------------------------------------------
    // read_content_preview
    // -------------------------------------------------------------

    #[test]
    fn read_preview_renders_content_lines() {
        let payload =
            r#"{"content":"line one\nline two","start_line":1,"end_line":2,"truncated":false}"#;
        let out = read_content_preview(payload).expect("preview");
        assert!(out.contains("  | line one"), "{out}");
        assert!(out.contains("  | line two"), "{out}");
    }

    #[test]
    fn read_preview_caps_and_notes_omissions() {
        let many: String = (0..(READ_PREVIEW_MAX_LINES + 7))
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let payload =
            format!(r#"{{"content":"{many}","start_line":1,"end_line":1,"truncated":false}}"#);
        let out = read_content_preview(&payload).expect("preview");
        assert!(out.contains("7 more lines omitted"), "{out}");
        assert!(out.lines().count() <= READ_PREVIEW_MAX_LINES + 1, "{out}");
    }

    #[test]
    fn read_preview_is_none_for_non_read_payloads() {
        // A list/search result has no `content` member.
        assert!(read_content_preview(r#"{"entries":[],"truncated":false}"#).is_none());
        assert!(read_content_preview("not json").is_none());
    }

    // -------------------------------------------------------------
    // read approval (outside-workspace reads)
    // -------------------------------------------------------------

    #[test]
    fn read_only_target_reads_the_path_field() {
        let read = ReadOnlyTool::Read {
            path: "../foo.txt".to_string(),
            line_range: None,
        };
        assert_eq!(read_only_target(&read), Some("../foo.txt"));
        let search = ReadOnlyTool::Search {
            pattern: "x".to_string(),
            path_prefix: None,
            case_sensitive: false,
            max_results: 10,
        };
        assert_eq!(read_only_target(&search), None);
    }

    #[test]
    fn read_extra_root_is_none_for_missing_path() {
        let root = std::env::temp_dir();
        let inv = ReadOnlyTool::Read {
            path: "definitely-missing-__attini__.txt".to_string(),
            line_range: None,
        };
        assert!(read_extra_root(&inv, &root).is_none());
    }

    #[test]
    fn read_extra_root_canonicalises_existing_path() {
        // The workspace root itself always exists; requesting it as a
        // read target yields a canonical extra root.
        let root = std::env::temp_dir();
        let expected = root.canonicalize().unwrap();
        let inv = ReadOnlyTool::List {
            path: ".".to_string(),
            recursive: false,
            max_entries: 10,
            include_hidden: false,
        };
        assert_eq!(read_extra_root(&inv, &root), Some(expected));
    }

    #[test]
    fn run_read_only_needs_approval_outside_workspace() {
        // A temp workspace with no granted roots; reading an absolute
        // path elsewhere on disk (the real cwd) is outside it.
        let workspace =
            std::env::temp_dir().join(format!("attini-read-approval-ws-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let executor = ToolExecutor::new(&workspace, Vec::new(), "t".to_string()).unwrap();
        let cwd_file = std::env::current_dir().unwrap().join("Cargo.toml");
        if !cwd_file.exists() {
            // Unexpected working directory; skip rather than false-fail.
            let _ = std::fs::remove_dir_all(&workspace);
            return;
        }
        let tc = ToolCall {
            id: "call_x".to_string(),
            function_name: "read".to_string(),
            arguments_json: format!(r#"{{"path":"{}"}}"#, cwd_file.display()),
        };
        match run_read_only(&tc, &executor, &[], &Authorization::PerTool) {
            ReadOnlyDispatch::NeedsApproval { summary, preview } => {
                assert!(summary.contains("approval required"), "{summary}");
                assert!(preview.contains("outside workspace"), "{preview}");
            }
            ReadOnlyDispatch::Done { content, .. } => {
                panic!("expected approval request, got: {content}")
            }
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn run_read_only_needs_approval_for_denied_workspace_path() {
        // Case 2: a `read` `allow:false` rule denies an in-workspace path,
        // so the read is parked for approval rather than silently allowed.
        let workspace =
            std::env::temp_dir().join(format!("attini-read-deny-ws-{}", std::process::id()));
        std::fs::create_dir_all(workspace.join("secret")).unwrap();
        std::fs::write(workspace.join("secret/data.txt"), "top secret").unwrap();
        let executor = ToolExecutor::new(&workspace, Vec::new(), "t".to_string()).unwrap();
        let rule = Rule::read(false, "secret".to_string());
        let layers = [(RuleScope::Workspace, std::slice::from_ref(&rule))];
        let tc = ToolCall {
            id: "call_y".to_string(),
            function_name: "read".to_string(),
            arguments_json: r#"{"path":"secret/data.txt"}"#.to_string(),
        };
        match run_read_only(&tc, &executor, &layers, &Authorization::PerTool) {
            ReadOnlyDispatch::NeedsApproval { summary, preview } => {
                assert!(summary.contains("approval required"), "{summary}");
                assert!(preview.contains("denied by a read rule"), "{preview}");
            }
            ReadOnlyDispatch::Done { content, .. } => {
                panic!("expected approval request, got: {content}")
            }
        }
        // Without the deny rule, the same read runs inline.
        match run_read_only(&tc, &executor, &[], &Authorization::PerTool) {
            ReadOnlyDispatch::Done {
                content, errored, ..
            } => {
                assert!(!errored);
                assert!(content.contains("top secret"), "{content}");
            }
            ReadOnlyDispatch::NeedsApproval { .. } => {
                panic!("expected inline read without the deny rule")
            }
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
