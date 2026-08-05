//! `attini session` subcommand implementations (list / show / tail
//! / rm / unlock / compact / prune).

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
}
