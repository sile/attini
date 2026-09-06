//! `attini session` subcommand implementations (list / show / tail
//! / rm / unlock / compact / prune).

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nojson::{DisplayJson, Json, JsonFormatter, RawJson};

use crate::agent_cli;
use crate::session::{
    ConversationSummary, LockStatus, Session, SessionPaths, inspect_lock, read_pending_summary,
    scan_conversation, session_paths, session_root,
};

pub fn run_list() -> io::Result<()> {
    let root = session_root();
    let entries = match fs::read_dir(&root) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("(no sessions in {})", root.display());
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mut rows: Vec<Row> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Ok(paths) = session_paths(&name) else {
            continue;
        };
        rows.push(build_row(name, &paths)?);
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    print_list_table(&rows);
    Ok(())
}

struct Row {
    name: String,
    lock: String,
    records: u64,
    pending: bool,
    updated_secs: Option<u64>,
}

fn build_row(name: String, paths: &SessionPaths) -> io::Result<Row> {
    let lock = format_lock_status(inspect_lock(&paths.lock));
    let summary = scan_conversation(&paths.conversation)?;
    let pending = paths.pending.try_exists()?;
    let updated_secs = fs::metadata(&paths.conversation)
        .or_else(|_| fs::metadata(&paths.dir))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Ok(Row {
        name,
        lock,
        records: summary.total_records,
        pending,
        updated_secs,
    })
}

fn print_list_table(rows: &[Row]) {
    println!(
        "{:<20}  {:<24}  {:>8}  {:<8}  UPDATED",
        "NAME", "LOCK", "RECORDS", "PENDING"
    );
    for r in rows {
        println!(
            "{:<20}  {:<24}  {:>8}  {:<8}  {}",
            r.name,
            r.lock,
            r.records,
            if r.pending { "yes" } else { "no" },
            r.updated_secs
                .map(format_unix_secs)
                .unwrap_or_else(|| "-".to_string()),
        );
    }
}

fn format_lock_status(status: LockStatus) -> String {
    match status {
        LockStatus::None => "none".to_string(),
        LockStatus::Corrupted => "broken".to_string(),
        LockStatus::PidDead => "stale".to_string(),
        LockStatus::PidAlive(pid) => format!("held(pid={pid})"),
    }
}

fn format_unix_secs(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(secs);
    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86_400 {
        format!("{}h ago", ago / 3600)
    } else {
        format!("{}d ago", ago / 86_400)
    }
}

pub fn run_show(name: &str) -> io::Result<()> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    let lock = inspect_lock(&paths.lock);
    let summary = scan_conversation(&paths.conversation)?;
    let pending = read_pending_summary(&paths.pending)?;

    println!("session: {name}");
    println!("  dir: {}", paths.dir.display());
    println!("  lock: {}", format_lock_status(lock));
    print_summary(&summary);
    match pending {
        Some(p) => {
            println!("  pending:");
            println!("    call_id: {}", p.call_id);
            println!("    tool_kind: {:?}", p.tool_kind);
            println!("    function_name: {}", p.function_name);
            println!("    ts: {}", p.ts);
            println!("    preview: {}", p.preview);
        }
        None => println!("  pending: (none)"),
    }
    Ok(())
}

fn print_summary(s: &ConversationSummary) {
    println!("  records: {}", s.total_records);
    println!(
        "  invocations: {} (completed={}, awaiting_approval={}, error={})",
        s.invocation_starts,
        s.invocation_ends_completed,
        s.invocation_ends_awaiting_approval,
        s.invocation_ends_error,
    );
    println!(
        "  messages: user={} assistant={} tool={} (assistant_tool_calls_total={})",
        s.user_messages, s.assistant_messages, s.tool_messages, s.assistant_tool_calls_total,
    );
    println!(
        "  approvals: approve={} reject={} (auto_approve={} auto_deny={})",
        s.approvals_approve, s.approvals_reject, s.approvals_auto_approve, s.approvals_auto_deny,
    );
    println!("  summaries: {}", s.summaries);
    match s.last_prompt_tokens {
        Some(pt) => {
            let hit = s
                .last_prompt_cache_hit_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let miss = s
                .last_prompt_cache_miss_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!("  token_usage: last prompt={pt} cache_hit={hit} cache_miss={miss}");
        }
        None => println!("  token_usage: (none)"),
    }
    match (s.last_ts, &s.last_kind) {
        (Some(ts), Some(kind)) => println!("  last_record: ts={ts} kind={kind}"),
        _ => println!("  last_record: (none)"),
    }
}

/// Read-only model summary of a session's current state. Never
/// writes to the conversation.
pub fn run_ask(
    name: &str,
    question: Option<&str>,
    model: &str,
    limit: Option<usize>,
    all: bool,
) -> io::Result<()> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    let mut records = crate::session::read_chat_message_with_ts(&paths.conversation, all)?;
    if let Some(limit) = limit {
        let start = records.len().saturating_sub(limit);
        records = records[start..].to_vec();
    }
    if records.is_empty() {
        println!("session {name:?}: no conversation records yet");
        return Ok(());
    }
    let text = agent_cli::run_ask_summary(records, model, question)?;
    println!("{text}");
    Ok(())
}

// -------------------------------------------------------------------
// attini session metrics
// -------------------------------------------------------------------

pub enum MetricsScope<'a> {
    /// Single session by name.
    Single(&'a str),
    /// All sessions under `.attini/` in the current directory.
    All,
}

pub fn run_metrics(scope: MetricsScope<'_>, json: bool) -> io::Result<()> {
    match scope {
        MetricsScope::Single(name) => {
            let m = collect_session_metrics(name)?;
            if json {
                println!("{}", Json(&SessionMetricsJson(&m)));
            } else {
                print_session_metrics_human(&m);
            }
        }
        MetricsScope::All => {
            let names = list_sessions()?;
            let mut per_session: Vec<PerSessionMetrics> = Vec::new();
            for name in names {
                match collect_session_metrics(&name) {
                    Ok(m) => per_session.push(m),
                    Err(e) => {
                        eprintln!("attini: skipping session {name:?}: {e}");
                    }
                }
            }
            if json {
                let total = compute_totals(&per_session);
                println!("{}", Json(&AllMetricsJson(&per_session, &total)));
            } else {
                print_all_metrics_human(&per_session);
            }
        }
    }
    Ok(())
}

/// Enumerate session names under `.attini/` in the current directory.
/// Names are returned sorted so the output is stable.
fn list_sessions() -> io::Result<Vec<String>> {
    let root = session_root();
    let entries = match fs::read_dir(&root) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string()
            && session_paths(&name).is_ok()
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[derive(Debug, Clone, Default)]
struct ToolCallCounts {
    list: u64,
    read: u64,
    search: u64,
    patch: u64,
    command: u64,
    skill_load: u64,
    subagent_run: u64,
    unknown: u64,
}

impl ToolCallCounts {
    fn total(&self) -> u64 {
        self.list
            + self.read
            + self.search
            + self.patch
            + self.command
            + self.skill_load
            + self.subagent_run
            + self.unknown
    }
}

#[derive(Debug, Clone, Default)]
struct MetricsAggregate {
    turns: u64,
    tool_calls: ToolCallCounts,
    tool_errors: u64,
    duration_ms_total: u64,
    prompt_tokens_billed_total: u64,
    completion_tokens_total: u64,
    prompt_cache_hit_tokens_total: u64,
    prompt_cache_miss_tokens_total: u64,
    compaction_attempts: u64,
    compaction_failures: u64,
}

#[derive(Debug, Clone, Default)]
struct TokenAggregate {
    prompt_tokens_billed_total: u64,
    completion_tokens_total: u64,
    prompt_cache_hit_tokens_total: u64,
    prompt_cache_miss_tokens_total: u64,
}

#[derive(Debug, Clone)]
struct PerSessionMetrics {
    session_name: String,
    summary: ConversationSummary,
    metrics: MetricsAggregate,
}

fn collect_session_metrics(name: &str) -> io::Result<PerSessionMetrics> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    let summary = scan_conversation(&paths.conversation)?;
    let (metrics_agg, token_agg) = scan_metrics_records(&paths.conversation)?;
    let mut merged = metrics_agg.clone();

    // Cross-check MetricsSnapshot token totals against the sum of
    // TokenUsage records at the session level. On divergence trust
    // TokenUsage (which is emitted per turn; MetricsSnapshot may be
    // missing on a crashed invocation).
    if metrics_agg.prompt_tokens_billed_total != token_agg.prompt_tokens_billed_total
        || metrics_agg.completion_tokens_total != token_agg.completion_tokens_total
        || metrics_agg.prompt_cache_hit_tokens_total != token_agg.prompt_cache_hit_tokens_total
        || metrics_agg.prompt_cache_miss_tokens_total != token_agg.prompt_cache_miss_tokens_total
    {
        eprintln!(
            "attini: session {name:?}: metrics_snapshot/token_usage token totals diverge, \
             using token_usage values (metrics_snapshot: prompt={} completion={}; \
             token_usage: prompt={} completion={})",
            metrics_agg.prompt_tokens_billed_total,
            metrics_agg.completion_tokens_total,
            token_agg.prompt_tokens_billed_total,
            token_agg.completion_tokens_total,
        );
        merged.prompt_tokens_billed_total = token_agg.prompt_tokens_billed_total;
        merged.completion_tokens_total = token_agg.completion_tokens_total;
        merged.prompt_cache_hit_tokens_total = token_agg.prompt_cache_hit_tokens_total;
        merged.prompt_cache_miss_tokens_total = token_agg.prompt_cache_miss_tokens_total;
    }

    Ok(PerSessionMetrics {
        session_name: name.to_string(),
        summary,
        metrics: merged,
    })
}

fn scan_metrics_records(path: &Path) -> io::Result<(MetricsAggregate, TokenAggregate)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((MetricsAggregate::default(), TokenAggregate::default()));
        }
        Err(e) => return Err(e),
    };
    let mut agg = MetricsAggregate::default();
    let mut tokens = TokenAggregate::default();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Err(e) = absorb_metrics_line(&line, &mut agg, &mut tokens) {
            return Err(io::Error::other(format!(
                "malformed record at line {}: {e}",
                i + 1
            )));
        }
    }
    Ok((agg, tokens))
}

fn absorb_metrics_line(
    line: &str,
    agg: &mut MetricsAggregate,
    tokens: &mut TokenAggregate,
) -> Result<(), String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    match kind.as_str() {
        "metrics_snapshot" => {
            let Some(counters) = value
                .to_member("counters")
                .map_err(|e| e.to_string())?
                .optional()
            else {
                return Ok(());
            };
            let get = |name: &str| -> Result<u64, String> {
                match counters
                    .to_member(name)
                    .map_err(|e| e.to_string())?
                    .optional()
                {
                    Some(v) => v
                        .try_into()
                        .map_err(|e: nojson::JsonParseError| e.to_string()),
                    None => Ok(0),
                }
            };
            agg.turns = agg.turns.saturating_add(get("turns")?);
            agg.tool_calls.list = agg.tool_calls.list.saturating_add(get("tool_calls.list")?);
            agg.tool_calls.read = agg.tool_calls.read.saturating_add(get("tool_calls.read")?);
            agg.tool_calls.search = agg
                .tool_calls
                .search
                .saturating_add(get("tool_calls.search")?);
            agg.tool_calls.patch = agg
                .tool_calls
                .patch
                .saturating_add(get("tool_calls.patch")?);
            agg.tool_calls.command = agg
                .tool_calls
                .command
                .saturating_add(get("tool_calls.command")?);
            agg.tool_calls.skill_load = agg
                .tool_calls
                .skill_load
                .saturating_add(get("tool_calls.skill_load")?);
            agg.tool_calls.subagent_run = agg
                .tool_calls
                .subagent_run
                .saturating_add(get("tool_calls.subagent_run")?);
            agg.tool_calls.unknown = agg
                .tool_calls
                .unknown
                .saturating_add(get("tool_calls.unknown")?);
            agg.tool_errors = agg.tool_errors.saturating_add(get("tool_errors")?);
            agg.duration_ms_total = agg.duration_ms_total.saturating_add(get("duration_ms")?);
            agg.prompt_tokens_billed_total = agg
                .prompt_tokens_billed_total
                .saturating_add(get("prompt_tokens_billed_total")?);
            agg.completion_tokens_total = agg
                .completion_tokens_total
                .saturating_add(get("completion_tokens_total")?);
            agg.prompt_cache_hit_tokens_total = agg
                .prompt_cache_hit_tokens_total
                .saturating_add(get("prompt_cache_hit_tokens_total")?);
            agg.prompt_cache_miss_tokens_total = agg
                .prompt_cache_miss_tokens_total
                .saturating_add(get("prompt_cache_miss_tokens_total")?);
            agg.compaction_attempts = agg
                .compaction_attempts
                .saturating_add(get("compaction_attempts")?);
            agg.compaction_failures = agg
                .compaction_failures
                .saturating_add(get("compaction_failures")?);
        }
        "token_usage" => {
            let Some(usage) = value
                .to_member("usage")
                .map_err(|e| e.to_string())?
                .optional()
            else {
                return Ok(());
            };
            let get = |name: &str| -> Result<u64, String> {
                match usage.to_member(name).map_err(|e| e.to_string())?.optional() {
                    Some(v) => v
                        .try_into()
                        .map_err(|e: nojson::JsonParseError| e.to_string()),
                    None => Ok(0),
                }
            };
            tokens.prompt_tokens_billed_total = tokens
                .prompt_tokens_billed_total
                .saturating_add(get("prompt_tokens")?);
            tokens.completion_tokens_total = tokens
                .completion_tokens_total
                .saturating_add(get("completion_tokens")?);
            tokens.prompt_cache_hit_tokens_total = tokens
                .prompt_cache_hit_tokens_total
                .saturating_add(get("prompt_cache_hit_tokens")?);
            tokens.prompt_cache_miss_tokens_total = tokens
                .prompt_cache_miss_tokens_total
                .saturating_add(get("prompt_cache_miss_tokens")?);
        }
        _ => {}
    }
    Ok(())
}

fn avg_duration_ms(m: &PerSessionMetrics) -> u64 {
    let starts = m.summary.invocation_starts;
    if starts == 0 {
        return 0;
    }
    m.metrics.duration_ms_total / starts
}

fn print_session_metrics_human(m: &PerSessionMetrics) {
    let s = &m.summary;
    let mx = &m.metrics;
    println!("session: {}", m.session_name);
    println!(
        "  invocations: {} (completed={}, awaiting_approval={}, error={})",
        s.invocation_starts,
        s.invocation_ends_completed,
        s.invocation_ends_awaiting_approval,
        s.invocation_ends_error,
    );
    println!("  turns: {}", mx.turns);
    println!("  tool_calls: {} total", mx.tool_calls.total());
    println!(
        "    list={}  read={}  search={}  patch={}  command={}  skill_load={}  \
         subagent_run={}  unknown={}",
        mx.tool_calls.list,
        mx.tool_calls.read,
        mx.tool_calls.search,
        mx.tool_calls.patch,
        mx.tool_calls.command,
        mx.tool_calls.skill_load,
        mx.tool_calls.subagent_run,
        mx.tool_calls.unknown,
    );
    println!("    errors={}", mx.tool_errors);
    println!(
        "  approvals: approve={} reject={} (auto_approve={} auto_deny={})",
        s.approvals_approve, s.approvals_reject, s.approvals_auto_approve, s.approvals_auto_deny,
    );
    println!("  tokens:");
    println!(
        "    prompt_billed={}  (cache_hit={} / miss={})",
        mx.prompt_tokens_billed_total,
        mx.prompt_cache_hit_tokens_total,
        mx.prompt_cache_miss_tokens_total,
    );
    println!("    completion={}", mx.completion_tokens_total);
    println!(
        "  compaction: attempts={} failures={}",
        mx.compaction_attempts, mx.compaction_failures,
    );
    println!(
        "  duration_ms: total={} avg_per_invocation={}",
        mx.duration_ms_total,
        avg_duration_ms(m),
    );
}

fn print_all_metrics_human(sessions: &[PerSessionMetrics]) {
    let headers = [
        "NAME",
        "INVOC",
        "TURNS",
        "TOOL_CALLS",
        "ERRS",
        "APPROVE/REJECT",
        "PROMPT_TOK",
        "COMPACT (attempt/fail)",
        "DURATION_MS",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();
    for m in sessions {
        let s = &m.summary;
        let mx = &m.metrics;
        rows.push(vec![
            m.session_name.clone(),
            s.invocation_starts.to_string(),
            mx.turns.to_string(),
            mx.tool_calls.total().to_string(),
            mx.tool_errors.to_string(),
            format!("{}/{}", s.approvals_approve, s.approvals_reject),
            mx.prompt_tokens_billed_total.to_string(),
            format!("{}/{}", mx.compaction_attempts, mx.compaction_failures),
            mx.duration_ms_total.to_string(),
        ]);
    }
    let total = compute_totals(sessions);
    let total_row = vec![
        String::new(),
        total.invocation_starts.to_string(),
        total.turns.to_string(),
        total.tool_calls_total.to_string(),
        total.tool_errors.to_string(),
        format!("{}/{}", total.approvals_approve, total.approvals_reject),
        total.prompt_tokens_billed_total.to_string(),
        format!(
            "{}/{}",
            total.compaction_attempts, total.compaction_failures
        ),
        total.duration_ms_total.to_string(),
    ];

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows.iter().chain(std::iter::once(&total_row)) {
        for (i, cell) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let render = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:width$}", c, width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let header_row: Vec<String> = headers.iter().map(|s| s.to_string()).collect();
    println!("{}", render(&header_row));
    for r in &rows {
        println!("{}", render(r));
    }
    let sep_len: usize = widths.iter().sum::<usize>() + 2 * (widths.len() - 1);
    println!(
        "--- total ({} sessions) {}",
        sessions.len(),
        "-".repeat(sep_len.saturating_sub(20 + sessions.len().to_string().len()))
    );
    println!("{}", render(&total_row));
}

#[derive(Debug, Clone, Default)]
struct TotalsAggregate {
    sessions_counted: u64,
    invocation_starts: u64,
    turns: u64,
    tool_calls_total: u64,
    tool_errors: u64,
    approvals_approve: u64,
    approvals_reject: u64,
    prompt_tokens_billed_total: u64,
    duration_ms_total: u64,
    compaction_attempts: u64,
    compaction_failures: u64,
}

fn compute_totals(sessions: &[PerSessionMetrics]) -> TotalsAggregate {
    let mut t = TotalsAggregate {
        sessions_counted: sessions.len() as u64,
        ..Default::default()
    };
    for m in sessions {
        t.invocation_starts = t
            .invocation_starts
            .saturating_add(m.summary.invocation_starts);
        t.turns = t.turns.saturating_add(m.metrics.turns);
        t.tool_calls_total = t
            .tool_calls_total
            .saturating_add(m.metrics.tool_calls.total());
        t.tool_errors = t.tool_errors.saturating_add(m.metrics.tool_errors);
        t.approvals_approve = t
            .approvals_approve
            .saturating_add(m.summary.approvals_approve);
        t.approvals_reject = t
            .approvals_reject
            .saturating_add(m.summary.approvals_reject);
        t.prompt_tokens_billed_total = t
            .prompt_tokens_billed_total
            .saturating_add(m.metrics.prompt_tokens_billed_total);
        t.duration_ms_total = t
            .duration_ms_total
            .saturating_add(m.metrics.duration_ms_total);
        t.compaction_attempts = t
            .compaction_attempts
            .saturating_add(m.metrics.compaction_attempts);
        t.compaction_failures = t
            .compaction_failures
            .saturating_add(m.metrics.compaction_failures);
    }
    t
}

// ---- JSON emitters ------------------------------------------------

struct SessionMetricsJson<'a>(&'a PerSessionMetrics);

impl DisplayJson for SessionMetricsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let m = self.0;
        let s = &m.summary;
        let mx = &m.metrics;
        f.object(|f| {
            f.member("session", &m.session_name)?;
            f.member(
                "invocations",
                &InvocationsJson {
                    starts: s.invocation_starts,
                    completed: s.invocation_ends_completed,
                    awaiting_approval: s.invocation_ends_awaiting_approval,
                    error: s.invocation_ends_error,
                },
            )?;
            f.member("turns", mx.turns)?;
            f.member("tool_calls", ToolCallsJson(&mx.tool_calls, mx.tool_errors))?;
            f.member(
                "approvals",
                &ApprovalsJson {
                    approve: s.approvals_approve,
                    reject: s.approvals_reject,
                    auto_approve: s.approvals_auto_approve,
                    auto_deny: s.approvals_auto_deny,
                },
            )?;
            f.member(
                "tokens",
                &TokensJson {
                    prompt_billed_total: mx.prompt_tokens_billed_total,
                    completion_total: mx.completion_tokens_total,
                    prompt_cache_hit_total: mx.prompt_cache_hit_tokens_total,
                    prompt_cache_miss_total: mx.prompt_cache_miss_tokens_total,
                },
            )?;
            f.member(
                "compaction",
                &CompactionJson {
                    attempts: mx.compaction_attempts,
                    failures: mx.compaction_failures,
                },
            )?;
            f.member(
                "duration_ms",
                &DurationJson {
                    total: mx.duration_ms_total,
                    avg_per_invocation: avg_duration_ms(m),
                },
            )
        })
    }
}

struct AllMetricsJson<'a>(&'a [PerSessionMetrics], &'a TotalsAggregate);

impl DisplayJson for AllMetricsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let sessions = self.0;
        let total = self.1;
        f.object(|f| {
            f.member(
                "sessions",
                sessions
                    .iter()
                    .map(SessionMetricsJson)
                    .collect::<Vec<_>>()
                    .as_slice(),
            )?;
            f.member("total", TotalsJson(total))
        })
    }
}

struct TotalsJson<'a>(&'a TotalsAggregate);

impl DisplayJson for TotalsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let t = self.0;
        f.object(|f| {
            f.member("sessions_counted", t.sessions_counted)?;
            f.member(
                "invocations",
                &TotalInvocationsJson {
                    starts: t.invocation_starts,
                },
            )?;
            f.member("turns", t.turns)?;
            f.member(
                "tool_calls",
                &TotalToolCallsJson {
                    total: t.tool_calls_total,
                    errors: t.tool_errors,
                },
            )?;
            f.member(
                "approvals",
                &TotalApprovalsJson {
                    approve: t.approvals_approve,
                    reject: t.approvals_reject,
                },
            )?;
            f.member(
                "tokens",
                &TotalTokensJson {
                    prompt_billed_total: t.prompt_tokens_billed_total,
                },
            )?;
            f.member(
                "compaction",
                &CompactionJson {
                    attempts: t.compaction_attempts,
                    failures: t.compaction_failures,
                },
            )?;
            f.member(
                "duration_ms",
                &TotalDurationJson {
                    total: t.duration_ms_total,
                },
            )
        })
    }
}

struct CompactionJson {
    attempts: u64,
    failures: u64,
}
impl DisplayJson for CompactionJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("attempts", self.attempts)?;
            f.member("failures", self.failures)
        })
    }
}

struct InvocationsJson {
    starts: u64,
    completed: u64,
    awaiting_approval: u64,
    error: u64,
}
impl DisplayJson for InvocationsJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("starts", self.starts)?;
            f.member("completed", self.completed)?;
            f.member("awaiting_approval", self.awaiting_approval)?;
            f.member("error", self.error)
        })
    }
}

struct ToolCallsJson<'a>(&'a ToolCallCounts, u64);
impl DisplayJson for ToolCallsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("total", self.0.total())?;
            f.member("by_kind", ToolCallsByKindJson(self.0))?;
            f.member("errors", self.1)
        })
    }
}

struct ToolCallsByKindJson<'a>(&'a ToolCallCounts);
impl DisplayJson for ToolCallsByKindJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("list", self.0.list)?;
            f.member("read", self.0.read)?;
            f.member("search", self.0.search)?;
            f.member("patch", self.0.patch)?;
            f.member("command", self.0.command)?;
            f.member("skill_load", self.0.skill_load)?;
            f.member("subagent_run", self.0.subagent_run)?;
            f.member("unknown", self.0.unknown)
        })
    }
}

struct ApprovalsJson {
    approve: u64,
    reject: u64,
    auto_approve: u64,
    auto_deny: u64,
}
impl DisplayJson for ApprovalsJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("approve", self.approve)?;
            f.member("reject", self.reject)?;
            f.member("auto_approve", self.auto_approve)?;
            f.member("auto_deny", self.auto_deny)
        })
    }
}

struct TokensJson {
    prompt_billed_total: u64,
    completion_total: u64,
    prompt_cache_hit_total: u64,
    prompt_cache_miss_total: u64,
}
impl DisplayJson for TokensJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("prompt_billed_total", self.prompt_billed_total)?;
            f.member("completion_total", self.completion_total)?;
            f.member("prompt_cache_hit_total", self.prompt_cache_hit_total)?;
            f.member("prompt_cache_miss_total", self.prompt_cache_miss_total)
        })
    }
}

struct DurationJson {
    total: u64,
    avg_per_invocation: u64,
}
impl DisplayJson for DurationJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("total", self.total)?;
            f.member("avg_per_invocation", self.avg_per_invocation)
        })
    }
}

struct TotalInvocationsJson {
    starts: u64,
}
impl DisplayJson for TotalInvocationsJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("starts", self.starts))
    }
}

struct TotalToolCallsJson {
    total: u64,
    errors: u64,
}
impl DisplayJson for TotalToolCallsJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("total", self.total)?;
            f.member("errors", self.errors)
        })
    }
}

struct TotalApprovalsJson {
    approve: u64,
    reject: u64,
}
impl DisplayJson for TotalApprovalsJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("approve", self.approve)?;
            f.member("reject", self.reject)
        })
    }
}

struct TotalTokensJson {
    prompt_billed_total: u64,
}
impl DisplayJson for TotalTokensJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("prompt_billed_total", self.prompt_billed_total))
    }
}

struct TotalDurationJson {
    total: u64,
}
impl DisplayJson for TotalDurationJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("total", self.total))
    }
}

pub fn run_prune(session_name: &str, yes: bool) -> io::Result<()> {
    let paths = session_paths(session_name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "session {session_name:?} not found ({})",
                paths.dir.display()
            ),
        ));
    }
    if let LockStatus::PidAlive(pid) = inspect_lock(&paths.lock) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "session {session_name:?} is held by pid {pid}; refusing to prune a running session"
            ),
        ));
    }

    let cutoff = match locate_last_summary_offset(&paths.conversation)? {
        Some(loc) => loc,
        None => {
            eprintln!("session {session_name:?}: no summary record found; nothing to prune");
            return Ok(());
        }
    };
    if cutoff.byte_offset == 0 {
        eprintln!(
            "session {session_name:?}: last summary is already at the start of the file; nothing to prune"
        );
        return Ok(());
    }

    let orig_size = fs::metadata(&paths.conversation)?.len();
    let dropped_records = cutoff.line_index; // number of lines strictly before the summary
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "session {session_name:?}: non-interactive prune requires --yes (stdin is not a TTY)"
                ),
            ));
        }
        print!(
            "Prune {} records before the last summary of {}? [y/N] ",
            dropped_records,
            paths.conversation.display()
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y") {
            eprintln!("cancelled");
            return Ok(());
        }
    }

    rewrite_from_offset(&paths.conversation, cutoff.byte_offset)?;
    let new_size = fs::metadata(&paths.conversation)?.len();
    eprintln!(
        "pruned: {dropped_records} records removed, {} {} -> {} bytes",
        paths.conversation.display(),
        orig_size,
        new_size
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct SummaryLocation {
    /// Byte offset of the last `summary` line in the file.
    byte_offset: u64,
    /// Number of lines strictly before that summary (i.e. the number
    /// of records prune would drop).
    line_index: u64,
}

fn locate_last_summary_offset(path: &Path) -> io::Result<Option<SummaryLocation>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::new(file);
    let mut offset: u64 = 0;
    let mut line_index: u64 = 0;
    let mut latest: Option<SummaryLocation> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let start = offset;
        let read_bytes = reader.read_line(&mut line)?;
        if read_bytes == 0 {
            break;
        }
        offset += read_bytes as u64;
        if line.trim().is_empty() {
            line_index += 1;
            continue;
        }
        if line_is_summary(&line) {
            latest = Some(SummaryLocation {
                byte_offset: start,
                line_index,
            });
        }
        line_index += 1;
    }
    Ok(latest)
}

fn line_is_summary(line: &str) -> bool {
    let json = match nojson::RawJson::parse(line) {
        Ok(j) => j,
        Err(_) => return false,
    };
    let kind = json
        .value()
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str());
    matches!(kind, Ok(ref s) if s.as_ref() == "summary")
}

fn rewrite_from_offset(path: &Path, offset: u64) -> io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    drop(file);

    let tmp = path.with_extension("jsonl.prune-tmp");
    // Best-effort cleanup of any leftover tmp from a previous crash.
    let _ = fs::remove_file(&tmp);
    {
        let mut out = File::create(&tmp)?;
        out.write_all(&buf)?;
        out.sync_all()?;
    }
    fs::rename(&tmp, path)
}

pub fn run_compact(session_name: &str, model: &str) -> io::Result<()> {
    let paths = session_paths(session_name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "session {session_name:?} not found ({})",
                paths.dir.display()
            ),
        ));
    }
    if let LockStatus::PidAlive(pid) = inspect_lock(&paths.lock) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "session {session_name:?} is held by pid {pid}; refusing to compact a running session"
            ),
        ));
    }
    if paths.pending.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "session {session_name:?} has pending.json; resume it with --approve / --reject before compacting"
            ),
        ));
    }
    let mut session = Session::open(session_name)?;
    let result = agent_cli::compact_conversation(&mut session, model);
    // Close explicitly so LOCK unlink errors are surfaced, but drop
    // ordering already covers the happy path.
    let _ = session.close();
    result
}

pub fn run_tail(name: &str, follow: bool, lines: usize) -> io::Result<()> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    let (tail_lines, cursor) = read_tail(&paths.conversation, lines)?;
    let mut stdout = io::stdout().lock();
    for line in &tail_lines {
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    if !follow {
        return Ok(());
    }
    follow_loop(&paths.conversation, cursor)
}

fn read_tail(path: &Path, lines: usize) -> io::Result<(Vec<String>, u64)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::new(file);
    let mut all: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        all.push(line.trim_end_matches('\n').to_string());
    }
    let cursor = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let start = all.len().saturating_sub(lines);
    Ok((all[start..].to_vec(), cursor))
}

fn follow_loop(path: &Path, mut cursor: u64) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    loop {
        thread::sleep(Duration::from_millis(500));
        let size = match fs::metadata(path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if size < cursor {
            // File truncated (unusual for JSONL append-only). Reset.
            cursor = 0;
        }
        if size == cursor {
            continue;
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(cursor))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        stdout.write_all(&buf)?;
        stdout.flush()?;
        cursor = size;
    }
}

pub fn run_rm(name: &str, yes: bool) -> io::Result<()> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    if let LockStatus::PidAlive(pid) = inspect_lock(&paths.lock) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session {name:?} is held by pid {pid}; refusing to remove a running session"),
        ));
    }
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session {name:?}: non-interactive rm requires --yes (stdin is not a TTY)"),
            ));
        }
        print!("Really remove {}? [y/N] ", paths.dir.display());
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y") {
            eprintln!("cancelled");
            return Ok(());
        }
    }
    fs::remove_dir_all(&paths.dir)?;
    eprintln!("removed {}", paths.dir.display());
    Ok(())
}

pub fn run_unlock(name: &str, force: bool) -> io::Result<()> {
    let paths = session_paths(name)?;
    match inspect_lock(&paths.lock) {
        LockStatus::None => {
            eprintln!("session {name:?}: no LOCK to remove");
            Ok(())
        }
        LockStatus::PidAlive(pid) if !force => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session {name:?} LOCK is held by live pid {pid}; pass --force to override"),
        )),
        LockStatus::PidAlive(_) | LockStatus::PidDead | LockStatus::Corrupted => {
            fs::remove_file(&paths.lock)?;
            eprintln!("removed {}", paths.lock.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("attini-prune-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("create tempdir");
        base
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = File::create(path).expect("create");
        for l in lines {
            f.write_all(l.as_bytes()).expect("write");
            f.write_all(b"\n").expect("newline");
        }
        f.sync_all().expect("sync");
    }

    #[test]
    fn line_is_summary_matches_only_summary_kind() {
        assert!(line_is_summary(
            r#"{"kind":"summary","ts":1,"since_ts":0,"cutoff_ts":0,"text":""}"#
        ));
        assert!(!line_is_summary(r#"{"kind":"user","ts":1,"text":"hi"}"#));
        // A user record whose text happens to contain the word summary
        // must NOT be misclassified as a summary.
        assert!(!line_is_summary(
            r#"{"kind":"user","ts":1,"text":"here is a summary of foo"}"#
        ));
        assert!(!line_is_summary("not json at all"));
        assert!(!line_is_summary(""));
    }

    #[test]
    fn locate_last_summary_offset_returns_none_when_no_summary() {
        let dir = tempdir("no_summary");
        let path = dir.join("conv.jsonl");
        write_lines(
            &path,
            &[
                r#"{"kind":"user","ts":1,"text":"a"}"#,
                r#"{"kind":"assistant","ts":2,"content":"b","reasoning":null,"tool_calls":[]}"#,
            ],
        );
        assert!(locate_last_summary_offset(&path).expect("ok").is_none());
    }

    #[test]
    fn locate_last_summary_offset_returns_offset_of_last_summary() {
        let dir = tempdir("last_summary");
        let path = dir.join("conv.jsonl");
        let lines = [
            r#"{"kind":"user","ts":1,"text":"first"}"#,
            r#"{"kind":"summary","ts":2,"since_ts":1,"cutoff_ts":1,"text":"s1"}"#,
            r#"{"kind":"user","ts":3,"text":"second"}"#,
            r#"{"kind":"summary","ts":4,"since_ts":3,"cutoff_ts":3,"text":"s2"}"#,
            r#"{"kind":"user","ts":5,"text":"third"}"#,
        ];
        write_lines(&path, &lines);
        let loc = locate_last_summary_offset(&path)
            .expect("ok")
            .expect("summary present");
        let expected_offset: u64 =
            (lines[0].len() + 1 + lines[1].len() + 1 + lines[2].len() + 1) as u64;
        assert_eq!(loc.byte_offset, expected_offset);
        assert_eq!(loc.line_index, 3); // three records precede the last summary
    }

    #[test]
    fn rewrite_from_offset_keeps_suffix_only() {
        let dir = tempdir("rewrite");
        let path = dir.join("conv.jsonl");
        let lines = [
            r#"{"kind":"user","ts":1,"text":"first"}"#,
            r#"{"kind":"summary","ts":2,"since_ts":1,"cutoff_ts":1,"text":"s1"}"#,
            r#"{"kind":"user","ts":3,"text":"after"}"#,
        ];
        write_lines(&path, &lines);
        let cutoff = locate_last_summary_offset(&path)
            .expect("ok")
            .expect("summary present");
        rewrite_from_offset(&path, cutoff.byte_offset).expect("rewrite ok");

        let contents = fs::read_to_string(&path).expect("read");
        let kept: Vec<&str> = contents.trim_end_matches('\n').split('\n').collect();
        assert_eq!(kept.len(), 2);
        assert!(kept[0].contains(r#""kind":"summary""#));
        assert!(kept[1].contains(r#""kind":"user""#));
        assert!(kept[1].contains("after"));
    }

    // ---- metrics scanner ---------------------------------------

    fn metrics_tempdir(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "attini-metrics-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("create tempdir");
        base
    }

    #[test]
    fn scan_metrics_records_aggregates_across_metrics_snapshots() {
        let dir = metrics_tempdir("multi_snapshot");
        let path = dir.join("conv.jsonl");
        write_lines(
            &path,
            &[
                r#"{"kind":"metrics_snapshot","ts":1,"counters":{"turns":2,"tool_calls.list":1,"tool_calls.read":3,"tool_errors":1,"duration_ms":100,"prompt_tokens_billed_total":500,"completion_tokens_total":40}}"#,
                r#"{"kind":"metrics_snapshot","ts":2,"counters":{"turns":1,"tool_calls.list":0,"tool_calls.read":0,"tool_calls.command":2,"tool_errors":0,"duration_ms":200,"prompt_tokens_billed_total":700,"completion_tokens_total":30}}"#,
            ],
        );
        let (agg, tokens) = scan_metrics_records(&path).expect("scan");
        assert_eq!(agg.turns, 3);
        assert_eq!(agg.tool_calls.list, 1);
        assert_eq!(agg.tool_calls.read, 3);
        assert_eq!(agg.tool_calls.command, 2);
        assert_eq!(agg.tool_calls.total(), 6);
        assert_eq!(agg.tool_errors, 1);
        assert_eq!(agg.duration_ms_total, 300);
        assert_eq!(agg.prompt_tokens_billed_total, 1200);
        assert_eq!(agg.completion_tokens_total, 70);
        // No token_usage records → tokens agg empty.
        assert_eq!(tokens.prompt_tokens_billed_total, 0);
    }

    #[test]
    fn scan_metrics_records_sums_token_usage_records() {
        let dir = metrics_tempdir("token_usage");
        let path = dir.join("conv.jsonl");
        write_lines(
            &path,
            &[
                r#"{"kind":"token_usage","ts":1,"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}}"#,
                r#"{"kind":"token_usage","ts":2,"usage":{"prompt_tokens":300,"completion_tokens":25,"prompt_cache_hit_tokens":250,"prompt_cache_miss_tokens":50}}"#,
                // token_usage without cache fields also aggregates.
                r#"{"kind":"token_usage","ts":3,"usage":{"prompt_tokens":50}}"#,
            ],
        );
        let (_agg, tokens) = scan_metrics_records(&path).expect("scan");
        assert_eq!(tokens.prompt_tokens_billed_total, 450);
        assert_eq!(tokens.completion_tokens_total, 35);
        assert_eq!(tokens.prompt_cache_hit_tokens_total, 330);
        assert_eq!(tokens.prompt_cache_miss_tokens_total, 70);
    }

    #[test]
    fn scan_metrics_records_ignores_unrelated_kinds() {
        let dir = metrics_tempdir("unrelated");
        let path = dir.join("conv.jsonl");
        write_lines(
            &path,
            &[
                r#"{"kind":"user","ts":1,"text":"hi"}"#,
                r#"{"kind":"assistant","ts":2,"content":"a","reasoning":null,"tool_calls":[]}"#,
                r#"{"kind":"summary","ts":3,"since_ts":0,"cutoff_ts":0,"text":"…"}"#,
                r#"{"kind":"tool_approval","ts":4,"call_id":"c","decision":"approve"}"#,
            ],
        );
        let (agg, tokens) = scan_metrics_records(&path).expect("scan");
        assert_eq!(agg.turns, 0);
        assert_eq!(agg.tool_calls.total(), 0);
        assert_eq!(tokens.prompt_tokens_billed_total, 0);
    }

    #[test]
    fn scan_metrics_records_missing_file_yields_zero_aggregates() {
        let dir = metrics_tempdir("missing");
        let path = dir.join("does-not-exist.jsonl");
        let (agg, tokens) = scan_metrics_records(&path).expect("scan");
        assert_eq!(agg.turns, 0);
        assert_eq!(agg.tool_calls.total(), 0);
        assert_eq!(tokens.prompt_tokens_billed_total, 0);
    }

    #[test]
    fn tool_call_counts_total_sums_all_kinds() {
        let counts = ToolCallCounts {
            list: 1,
            read: 2,
            search: 3,
            patch: 4,
            command: 5,
            skill_load: 6,
            subagent_run: 7,
            unknown: 9,
        };
        assert_eq!(counts.total(), 37);
    }

    #[test]
    fn compute_totals_saturates_over_multi_session_input() {
        let m1 = PerSessionMetrics {
            session_name: "a".to_string(),
            summary: ConversationSummary {
                invocation_starts: 2,
                approvals_approve: 3,
                approvals_reject: 1,
                ..Default::default()
            },
            metrics: MetricsAggregate {
                turns: 5,
                tool_calls: ToolCallCounts {
                    list: 1,
                    read: 1,
                    ..Default::default()
                },
                tool_errors: 2,
                duration_ms_total: 1000,
                prompt_tokens_billed_total: 5000,
                compaction_attempts: 4,
                compaction_failures: 1,
                ..Default::default()
            },
        };
        let m2 = PerSessionMetrics {
            session_name: "b".to_string(),
            summary: ConversationSummary {
                invocation_starts: 1,
                approvals_approve: 1,
                ..Default::default()
            },
            metrics: MetricsAggregate {
                turns: 3,
                tool_calls: ToolCallCounts {
                    command: 4,
                    ..Default::default()
                },
                tool_errors: 0,
                duration_ms_total: 500,
                prompt_tokens_billed_total: 1000,
                compaction_attempts: 2,
                compaction_failures: 2,
                ..Default::default()
            },
        };
        let t = compute_totals(&[m1, m2]);
        assert_eq!(t.sessions_counted, 2);
        assert_eq!(t.invocation_starts, 3);
        assert_eq!(t.turns, 8);
        assert_eq!(t.tool_calls_total, 6);
        assert_eq!(t.tool_errors, 2);
        assert_eq!(t.approvals_approve, 4);
        assert_eq!(t.approvals_reject, 1);
        assert_eq!(t.prompt_tokens_billed_total, 6000);
        assert_eq!(t.duration_ms_total, 1500);
        assert_eq!(t.compaction_attempts, 6);
        assert_eq!(t.compaction_failures, 3);
    }

    #[test]
    fn absorb_metrics_line_picks_up_compaction_counters() {
        let mut agg = MetricsAggregate::default();
        let mut tokens = TokenAggregate::default();
        let line = r#"{"kind":"metrics_snapshot","ts":0,"counters":{"compaction_attempts":5,"compaction_failures":2}}"#;
        absorb_metrics_line(line, &mut agg, &mut tokens).expect("parse ok");
        assert_eq!(agg.compaction_attempts, 5);
        assert_eq!(agg.compaction_failures, 2);
    }

    #[test]
    fn absorb_metrics_line_defaults_missing_compaction_counters_to_zero() {
        let mut agg = MetricsAggregate::default();
        let mut tokens = TokenAggregate::default();
        let line = r#"{"kind":"metrics_snapshot","ts":0,"counters":{"turns":1}}"#;
        absorb_metrics_line(line, &mut agg, &mut tokens).expect("parse ok");
        assert_eq!(agg.turns, 1);
        assert_eq!(agg.compaction_attempts, 0);
        assert_eq!(agg.compaction_failures, 0);
    }
}
