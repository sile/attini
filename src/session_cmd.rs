//! Top-level session inspection subcommands (status / logstats).
//! Session data lives under `.attini/<NAME>/`; attini
//! keeps no abstraction over it, so reading `conversation.jsonl` is done
//! directly on the filesystem.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use nojson::{DisplayJson, Json, JsonFormatter, RawJson};

use crate::session::{
    CommandFamilyStats, ConversationAnalysis, ConversationSummary, LockStatus, PendingSummary,
    ProgramStats, ReadTargetStats, RecordKindBytes, SessionPaths, SummaryBytes,
    TokenUsageAggregate, ToolResultStats, analyze_conversation, inspect_lock, read_pending_summary,
    scan_conversation, session_paths,
};
use crate::tell_cli;

fn format_lock_status(status: LockStatus) -> String {
    match status {
        LockStatus::None => "none".to_string(),
        LockStatus::Corrupted => "broken".to_string(),
        LockStatus::PidDead => "stale".to_string(),
        LockStatus::PidAlive(pid) => format!("held(pid={pid})"),
    }
}

/// Read-only overview of a session: current state (lock, pending
/// calls, summary) plus aggregate metrics. The successor to the old
/// `show` + `metrics` pair.
pub fn run_status(name: &str, json: bool) -> io::Result<()> {
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
    let metrics = collect_session_metrics(name)?;

    if json {
        println!(
            "{}",
            Json(&StatusJson {
                name,
                paths: &paths,
                lock,
                summary: &summary,
                pending: pending.as_deref(),
                metrics: &metrics,
            })
        );
        return Ok(());
    }

    println!("session: {name}");
    println!("  dir: {}", paths.dir.display());
    println!("  lock: {}", format_lock_status(lock));
    print_summary(&summary);
    match pending {
        Some(ps) => {
            println!("  pending ({}):", ps.len());
            for p in ps {
                println!("    call_id: {}", p.call_id);
                println!("    tool_kind: {:?}", p.tool_kind);
                println!("    function_name: {}", p.function_name);
                println!("    ts: {}", p.ts);
                println!("    preview: {}", p.preview);
            }
        }
        None => println!("  pending: (none)"),
    }
    print_session_metrics_human_tail(&metrics);
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
    max_tokens: Option<u64>,
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
    let fingerprint = crate::session::conversation_fingerprint(&records);
    let prior = crate::session::load_ask_state(&paths.ask)?;
    let mut state = crate::session::AskState {
        conversation_fingerprint: fingerprint,
        entries: Vec::new(),
    };
    if let Some(prev) = prior {
        if prev.conversation_fingerprint == fingerprint && !prev.entries.is_empty() {
            state.entries = prev.entries;
        } else if prev.conversation_fingerprint != fingerprint {
            println!("(prior ask context reset: the observed conversation changed)");
        }
    }
    let prior_text = render_prior_ask_context(&state.entries);
    let text =
        tell_cli::run_ask_summary(records, model, question, prior_text.as_deref(), max_tokens)?;
    println!("{text}");
    state.entries.push(crate::session::AskEntry {
        ts: crate::session::now_unix_millis(),
        question: question.map(|q| q.to_string()),
        answer: text,
    });
    if state.entries.len() > MAX_ASK_ENTRIES {
        let start = state.entries.len() - MAX_ASK_ENTRIES;
        state.entries = state.entries[start..].to_vec();
    }
    crate::session::save_ask_state(&paths.ask, &state)?;
    Ok(())
}

/// Maximum number of Q&A entries kept in `ask.json`. Only the most
/// recent [`PRIOR_ASK_CONTEXT_ENTRIES`] are injected into the next
/// `ask`, but a few more are retained so repeated `ask` runs have a
/// small working window before a conversation change clears them.
const MAX_ASK_ENTRIES: usize = 20;
/// Number of previous entries injected as prior context into the next
/// `attini ask`.
const PRIOR_ASK_CONTEXT_ENTRIES: usize = 2;
/// Cap on each prior answer's length when injected, so context stays
/// a hint rather than a transcript.
const PRIOR_ANSWER_MAX_CHARS: usize = 2000;

/// Render the most recent [`PRIOR_ASK_CONTEXT_ENTRIES`] cached
/// Q&A entries as a compact hint block for the next `ask`. Returns
/// `None` when there is no usable prior context.
fn render_prior_ask_context(entries: &[crate::session::AskEntry]) -> Option<String> {
    let tail = entries.len().saturating_sub(PRIOR_ASK_CONTEXT_ENTRIES);
    let recent = &entries[tail..];
    if recent.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, entry) in recent.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        match &entry.question {
            Some(q) => out.push_str(&format!("Q: {q}\n")),
            None => out.push_str("Q: (no question; a status summary)\n"),
        }
        let mut answer = entry.answer.clone();
        if answer.chars().count() > PRIOR_ANSWER_MAX_CHARS {
            answer = answer
                .chars()
                .take(PRIOR_ANSWER_MAX_CHARS)
                .collect::<String>();
            answer.push('…');
        }
        out.push_str(&format!("A: {answer}"));
    }
    Some(out)
}

#[derive(Debug, Clone, Default)]
struct ToolCallCounts {
    list: u64,
    read: u64,
    search: u64,
    patch: u64,
    command: u64,
    unknown: u64,
}

impl ToolCallCounts {
    fn total(&self) -> u64 {
        self.list + self.read + self.search + self.patch + self.command + self.unknown
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

/// Metrics portion of `attini status`. Skips `session` / `invocations`
/// / `approvals`, which `print_summary` already emitted, so the human
/// output has no duplicated lines.
fn print_session_metrics_human_tail(m: &PerSessionMetrics) {
    let mx = &m.metrics;
    println!("  turns: {}", mx.turns);
    println!("  tool_calls: {} total", mx.tool_calls.total());
    println!(
        "    list={}  read={}  search={}  patch={}  command={}  unknown={}",
        mx.tool_calls.list,
        mx.tool_calls.read,
        mx.tool_calls.search,
        mx.tool_calls.patch,
        mx.tool_calls.command,
        mx.tool_calls.unknown,
    );
    println!("    errors={}", mx.tool_errors);
    println!("  tokens (billed):");
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

pub fn run_logstats(name: &str, json: bool) -> io::Result<()> {
    let paths = session_paths(name)?;
    if !paths.dir.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("session {name:?} not found ({})", paths.dir.display()),
        ));
    }
    let analysis = analyze_conversation(&paths.conversation)?;
    if json {
        println!("{}", Json(&LogstatsJson(&analysis, name)));
    } else {
        print_logstats_human(name, &paths, &analysis);
    }
    Ok(())
}

const LOGSTATS_TOP_N: usize = 10;

fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2} MiB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.2} KiB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

fn print_logstats_human(name: &str, paths: &SessionPaths, a: &ConversationAnalysis) {
    println!("session: {}", name);
    println!("path: {}", paths.conversation.display());
    println!(
        "records: {}   total: {} ({})",
        a.records,
        a.total_bytes,
        format_bytes(a.total_bytes)
    );

    println!("\nrecord kind histogram");
    let mut kinds = a.kind_bytes.clone();
    kinds.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
    print_two_col(
        "KIND",
        "COUNT / BYTES",
        &kinds
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    format!("{} / {}", s.count, format_bytes(s.bytes)),
                )
            })
            .collect::<Vec<_>>(),
    );

    println!("\nassistant payload");
    println!("  content: {}", format_bytes(a.assistant_content_bytes));
    println!("  tool_calls: {}", a.tool_calls_count);

    println!("\ntool results by function");
    let mut tools = a.tool_results.clone();
    tools.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
    print_three_col(
        "FUNCTION",
        "COUNT",
        "BYTES / MAX",
        &tools
            .iter()
            .take(LOGSTATS_TOP_N)
            .map(|(k, s)| {
                (
                    k.clone(),
                    s.count.to_string(),
                    format!("{} / {}", format_bytes(s.bytes), format_bytes(s.max_bytes)),
                )
            })
            .collect::<Vec<_>>(),
    );
    if tools.len() > LOGSTATS_TOP_N {
        println!("  ... {} more function(s)", tools.len() - LOGSTATS_TOP_N);
    }

    let read_rows: Vec<(String, String)> = a
        .read_targets
        .iter()
        .map(|(p, s)| {
            (
                p.clone(),
                format!(
                    "{} calls / {} bytes / {} ranges / max {}",
                    s.calls,
                    format_bytes(s.bytes),
                    s.ranges,
                    format_bytes(s.max_bytes)
                ),
            )
        })
        .collect();
    println!(
        "\nread targets, top {}",
        LOGSTATS_TOP_N.min(read_rows.len())
    );
    let mut reads = a.read_targets.clone();
    reads.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
    print_three_col(
        "PATH",
        "CASES",
        "BYTES / MAX / RANGES",
        &reads
            .iter()
            .take(LOGSTATS_TOP_N)
            .map(|(k, s)| {
                (
                    k.clone(),
                    s.calls.to_string(),
                    format!(
                        "{} / {} / {}",
                        format_bytes(s.bytes),
                        format_bytes(s.max_bytes),
                        s.ranges
                    ),
                )
            })
            .collect::<Vec<_>>(),
    );
    if reads.len() > LOGSTATS_TOP_N {
        println!("  ... {} more path(s)", reads.len() - LOGSTATS_TOP_N);
    }

    let mut programs = a.programs.clone();
    programs.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
    println!("\nprograms, top {}", LOGSTATS_TOP_N.min(programs.len()));
    print_three_col(
        "PROGRAM",
        "COUNT",
        "BYTES",
        &programs
            .iter()
            .take(LOGSTATS_TOP_N)
            .map(|(k, s)| (k.clone(), s.count.to_string(), format_bytes(s.bytes)))
            .collect::<Vec<_>>(),
    );
    if programs.len() > LOGSTATS_TOP_N {
        println!("  ... {} more program(s)", programs.len() - LOGSTATS_TOP_N);
    }

    let mut fams = a.command_families.clone();
    fams.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
    println!(
        "\ncommand families (argv[0] x argv[1]), top {}",
        LOGSTATS_TOP_N.min(fams.len())
    );
    print_three_col(
        "(PROGRAM, SUBCOMMAND)",
        "COUNT",
        "BYTES",
        &fams
            .iter()
            .take(LOGSTATS_TOP_N)
            .map(|(fam, st)| {
                (
                    format!(
                        "{}, {}",
                        fam.program,
                        fam.subcommand.as_deref().unwrap_or("-")
                    ),
                    st.count.to_string(),
                    format_bytes(st.bytes),
                )
            })
            .collect::<Vec<_>>(),
    );
    if fams.len() > LOGSTATS_TOP_N {
        println!("  ... {} more family(ies)", fams.len() - LOGSTATS_TOP_N);
    }

    let t = &a.token_usage;
    println!("\ntoken usage");
    println!(
        "  prompt: total {} / max {} / latest {}",
        format_bytes(t.prompt_total),
        format_bytes(t.prompt_max),
        t.latest
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("  completion: total {}", format_bytes(t.completion_total));
    println!(
        "  cache hit/miss: {} / {}",
        format_bytes(t.cache_hit_total),
        format_bytes(t.cache_miss_total)
    );
    println!("  records: {}", t.records);

    if a.summary_text.count > 0 {
        println!(
            "\nsummary text: {} ({} records)",
            format_bytes(a.summary_text.bytes),
            a.summary_text.count
        );
    } else {
        println!("\nsummary text: none");
    }
}

fn print_two_col(header: &str, header2: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        println!("  (none)");
        return;
    }
    let w1 = header
        .len()
        .max(rows.iter().map(|(a, _)| a.len()).max().unwrap_or(0));
    println!("{:<w1$}  {}", header, header2);
    for (a, b) in rows {
        println!("{:<w1$}  {}", a, b);
    }
}

fn print_three_col(header: &str, header2: &str, header3: &str, rows: &[(String, String, String)]) {
    if rows.is_empty() {
        println!("  (none)");
        return;
    }
    let w1 = header
        .len()
        .max(rows.iter().map(|(a, _, _)| a.len()).max().unwrap_or(0));
    let w2 = header2
        .len()
        .max(rows.iter().map(|(_, b, _)| b.len()).max().unwrap_or(0));
    println!("{:<w1$}  {:<w2$}  {}", header, header2, header3);
    for (a, b, c) in rows {
        println!("{:<w1$}  {:<w2$}  {}", a, b, c);
    }
}

/// `--json` renderer for `attini logstats`. Emits the entire
/// analysis (not just the top-N) so the output is a complete,
/// diffable baseline.
struct LogstatsJson<'a>(&'a ConversationAnalysis, &'a str);

impl DisplayJson for LogstatsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let a = self.0;
        let name = self.1;
        f.object(|f| {
            f.member("session", name)?;
            f.member("records", a.records)?;
            f.member("total_bytes", a.total_bytes)?;

            let mut kinds = a.kind_bytes.clone();
            kinds.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
            f.member("kinds", KindsJson(&kinds))?;

            f.member(
                "assistant",
                AssistantJson {
                    content_bytes: a.assistant_content_bytes,
                    tool_calls: a.tool_calls_count,
                },
            )?;

            let mut tools = a.tool_results.clone();
            tools.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
            f.member("tool_results", ToolResultsJson(&tools))?;

            let mut reads = a.read_targets.clone();
            reads.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
            f.member("read_targets", ReadTargetsJson(&reads))?;

            let mut programs = a.programs.clone();
            programs.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
            f.member("programs", ProgramsJson(&programs))?;

            let mut fams = a.command_families.clone();
            fams.sort_by_key(|x| std::cmp::Reverse(x.1.bytes));
            f.member(
                "command_families",
                fams.iter()
                    .map(|(fam, s)| FamilyEntryJson {
                        program: &fam.program,
                        subcommand: fam.subcommand.as_deref(),
                        stats: s,
                    })
                    .collect::<Vec<_>>(),
            )?;

            f.member("token_usage", a.token_usage)?;
            f.member("summary_text", &a.summary_text)?;
            Ok(())
        })
    }
}

/// Wrappers that render maps / collections as JSON objects inside a
/// larger object. `nojson`'s `JsonObjectFormatter` has no nested
/// `object(name, ..)` method, so each map gets its own `DisplayJson`
/// view.
struct KindsJson<'a>(&'a [(String, RecordKindBytes)]);

impl DisplayJson for KindsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            for (k, s) in self.0 {
                f.member(k, s)?;
            }
            Ok(())
        })
    }
}

struct AssistantJson {
    content_bytes: u64,
    tool_calls: u64,
}

impl DisplayJson for AssistantJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("content_bytes", self.content_bytes)?;
            f.member("tool_calls", self.tool_calls)?;
            Ok(())
        })
    }
}

struct ToolResultsJson<'a>(&'a [(String, ToolResultStats)]);

impl DisplayJson for ToolResultsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            for (k, s) in self.0 {
                f.member(k, s)?;
            }
            Ok(())
        })
    }
}

struct ReadTargetsJson<'a>(&'a [(String, ReadTargetStats)]);

impl DisplayJson for ReadTargetsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            for (k, s) in self.0 {
                f.member(k, s)?;
            }
            Ok(())
        })
    }
}

struct ProgramsJson<'a>(&'a [(String, ProgramStats)]);

impl DisplayJson for ProgramsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            for (k, s) in self.0 {
                f.member(k, s)?;
            }
            Ok(())
        })
    }
}

struct FamilyEntryJson<'a> {
    program: &'a str,
    subcommand: Option<&'a str>,
    stats: &'a CommandFamilyStats,
}

impl DisplayJson for FamilyEntryJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("program", self.program)?;
            f.member("subcommand", self.subcommand)?;
            f.member("count", self.stats.count)?;
            f.member("bytes", self.stats.bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for RecordKindBytes {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("count", self.count)?;
            f.member("bytes", self.bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for ReadTargetStats {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("calls", self.calls)?;
            f.member("bytes", self.bytes)?;
            f.member("max_bytes", self.max_bytes)?;
            f.member("ranges", self.ranges)?;
            Ok(())
        })
    }
}

impl DisplayJson for CommandFamilyStats {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("count", self.count)?;
            f.member("bytes", self.bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for ProgramStats {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("count", self.count)?;
            f.member("bytes", self.bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for ToolResultStats {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("count", self.count)?;
            f.member("bytes", self.bytes)?;
            f.member("max_bytes", self.max_bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for SummaryBytes {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("count", self.count)?;
            f.member("bytes", self.bytes)?;
            Ok(())
        })
    }
}

impl DisplayJson for TokenUsageAggregate {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("records", self.records)?;
            f.member("prompt_total", self.prompt_total)?;
            f.member("completion_total", self.completion_total)?;
            f.member("cache_hit_total", self.cache_hit_total)?;
            f.member("cache_miss_total", self.cache_miss_total)?;
            f.member("prompt_max", self.prompt_max)?;
            f.member("latest", self.latest)?;
            Ok(())
        })
    }
}

// ---- JSON emitters ------------------------------------------------

/// `--json` renderer for `attini status`. Combines the current-state
/// overview (lock / summary / pending) with the aggregate metrics in
/// a single object.
///
/// Facts that the summary already carries (invocations, approvals) are
/// emitted once here and not repeated under `metrics`. `summary`
/// holds the raw count of the log; `metrics` holds what is computed
/// from it (turns, tool calls, totals, compaction, duration).
struct StatusJson<'a> {
    name: &'a str,
    paths: &'a SessionPaths,
    lock: LockStatus,
    summary: &'a ConversationSummary,
    pending: Option<&'a [PendingSummary]>,
    metrics: &'a PerSessionMetrics,
}

impl DisplayJson for StatusJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("session", self.name)?;
            f.member("dir", self.paths.dir.to_string_lossy().as_ref())?;
            f.member("lock", lock_status_str(self.lock))?;
            f.member("summary", SummaryJson(self.summary))?;
            match self.pending {
                Some(ps) => {
                    let items: Vec<PendingJson<'_>> = ps.iter().map(PendingJson).collect();
                    f.member("pending", items.as_slice())?;
                }
                None => f.member("pending", Option::<PendingJson<'_>>::None)?,
            }
            f.member("metrics", SessionMetricsJson(self.metrics))
        })
    }
}

fn lock_status_str(status: LockStatus) -> &'static str {
    match status {
        LockStatus::None => "none",
        LockStatus::Corrupted => "broken",
        LockStatus::PidDead => "stale",
        LockStatus::PidAlive(_) => "held",
    }
}

struct SummaryJson<'a>(&'a ConversationSummary);

impl DisplayJson for SummaryJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let s = self.0;
        f.object(|f| {
            f.member("records", s.total_records)?;
            f.member(
                "invocations",
                &InvocationsJson {
                    starts: s.invocation_starts,
                    completed: s.invocation_ends_completed,
                    awaiting_approval: s.invocation_ends_awaiting_approval,
                    error: s.invocation_ends_error,
                },
            )?;
            f.member(
                "messages",
                &MessagesJson {
                    user: s.user_messages,
                    assistant: s.assistant_messages,
                    tool: s.tool_messages,
                    assistant_tool_calls_total: s.assistant_tool_calls_total,
                },
            )?;
            f.member(
                "approvals",
                &ApprovalsJson {
                    approve: s.approvals_approve,
                    reject: s.approvals_reject,
                    auto_approve: s.approvals_auto_approve,
                    auto_deny: s.approvals_auto_deny,
                },
            )?;
            f.member("summaries", s.summaries)?;
            f.member(
                "token_usage",
                &LastTokenUsageJson {
                    prompt: s.last_prompt_tokens,
                    cache_hit: s.last_prompt_cache_hit_tokens,
                    cache_miss: s.last_prompt_cache_miss_tokens,
                },
            )?;
            f.member(
                "last_record",
                &LastRecordJson {
                    ts: s.last_ts,
                    kind: s.last_kind.as_deref(),
                },
            )
        })
    }
}

struct MessagesJson {
    user: u64,
    assistant: u64,
    tool: u64,
    assistant_tool_calls_total: u64,
}
impl DisplayJson for MessagesJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("user", self.user)?;
            f.member("assistant", self.assistant)?;
            f.member("tool", self.tool)?;
            f.member(
                "assistant_tool_calls_total",
                self.assistant_tool_calls_total,
            )
        })
    }
}

/// The *last* turn's token usage (not a total). Contrast with
/// `TokensJson` under `metrics.tokens_total`, which is cumulative.
struct LastTokenUsageJson {
    prompt: Option<u64>,
    cache_hit: Option<u64>,
    cache_miss: Option<u64>,
}
impl DisplayJson for LastTokenUsageJson {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("last_prompt", self.prompt)?;
            f.member("cache_hit", self.cache_hit)?;
            f.member("cache_miss", self.cache_miss)
        })
    }
}

struct LastRecordJson<'a> {
    ts: Option<u64>,
    kind: Option<&'a str>,
}
impl DisplayJson for LastRecordJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("ts", self.ts)?;
            f.member("kind", self.kind)
        })
    }
}

struct PendingJson<'a>(&'a PendingSummary);

impl DisplayJson for PendingJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let p = self.0;
        f.object(|f| {
            f.member("call_id", p.call_id.as_str())?;
            f.member("tool_kind", pending_tool_kind_str(p.tool_kind))?;
            f.member("function_name", p.function_name.as_str())?;
            f.member("ts", p.ts)?;
            f.member("preview", p.preview.as_str())
        })
    }
}

fn pending_tool_kind_str(kind: crate::session::PendingToolKind) -> &'static str {
    match kind {
        crate::session::PendingToolKind::Patch => "patch",
        crate::session::PendingToolKind::Command => "command",
    }
}

struct SessionMetricsJson<'a>(&'a PerSessionMetrics);

impl DisplayJson for SessionMetricsJson<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        let m = self.0;
        let mx = &m.metrics;
        // NOTE: `invocations` and `approvals` are deliberately omitted here;
        // they are emitted once, under `summary`, by `StatusJson`. This keeps
        // the JSON free of the same fact stored under two names, matching the
        // human output (`print_session_metrics_human_tail`).
        f.object(|f| {
            f.member("turns", mx.turns)?;
            f.member("tool_calls", ToolCallsJson(&mx.tool_calls, mx.tool_errors))?;
            f.member(
                "tokens_total",
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

/// Cumulative token totals across every invocation. Contrast with
/// `SummaryJson.token_usage`, which is the *last* turn's usage only.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;

    #[test]
    fn render_prior_ask_context_takes_last_two_and_truncates_answers() {
        let entries = vec![
            crate::session::AskEntry {
                ts: 1,
                question: Some("q1".to_string()),
                answer: "a1".to_string(),
            },
            crate::session::AskEntry {
                ts: 2,
                question: None,
                answer: "a2".to_string(),
            },
            crate::session::AskEntry {
                ts: 3,
                question: Some("q3".to_string()),
                answer: "x".repeat(PRIOR_ANSWER_MAX_CHARS + 50),
            },
        ];
        let rendered = render_prior_ask_context(&entries).expect("prior context");
        // Only the last two entries (ts 2, ts 3) are rendered.
        assert!(rendered.contains("(no question; a status summary)"));
        assert!(rendered.contains("Q: q3"));
        assert!(!rendered.contains("Q: q1"));
        // The over-long answer is truncated to the cap plus an ellipsis.
        assert!(rendered.contains(&"x".repeat(PRIOR_ANSWER_MAX_CHARS)));
        assert!(!rendered.contains(&"x".repeat(PRIOR_ANSWER_MAX_CHARS + 50)));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn render_prior_ask_context_none_when_empty() {
        assert!(render_prior_ask_context(&[]).is_none());
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = File::create(path).expect("create");
        for l in lines {
            f.write_all(l.as_bytes()).expect("write");
            f.write_all(b"\n").expect("newline");
        }
        f.sync_all().expect("sync");
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
                r#"{"kind":"assistant","ts":2,"content":"a","tool_calls":[]}"#,
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
            unknown: 9,
        };
        assert_eq!(counts.total(), 24);
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
