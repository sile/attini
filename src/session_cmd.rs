//! `attini session` subcommand implementations (list / show / tail
//! / rm / unlock / compact).

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
