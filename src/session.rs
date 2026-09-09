//! Filesystem-backed session storage for the CLI agent
//! (`attini agent`).
//!
//! Each session lives under `.attini/{SESSION_NAME}/` relative to
//! the current working directory:
//!
//! - `LOCK` — exclusive lock file created with `O_EXCL` on open;
//!   contains `{"pid": i32, "started_at_unix_ms": u64}` of the
//!   holder. Removed on `Session::close` / `Drop`. Stale LOCKs
//!   (holder PID dead or file corrupted) are auto-recovered by
//!   one retry on the next `Session::open`.
//! - `conversation.jsonl` — append-only history. One JSON object
//!   per line, tagged by `kind`. User / assistant / tool records
//!   are the source of truth for reconstructing conversation
//!   context on subsequent invocations.
//! - `pending.json` — present iff the previous invocation
//!   suspended waiting for approval. Absent otherwise.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nojson::{DisplayJson, Json, JsonFormatter, JsonParseError, RawJson};

use crate::sansio::deepseek::{ChatMessage, ThinkingEffort, ToolCall};

/// Handle to an open session directory. Holds the LOCK file open
/// for the lifetime of this value; drop it via [`Session::close`]
/// to release the lock.
pub struct Session {
    dir: PathBuf,
    lock_path: PathBuf,
    _lock: File,
    conversation_path: PathBuf,
    pending_path: PathBuf,
    writer: File,
    /// Whether plan mode is on for this session: when `true`, every
    /// patch (including git-tracked edits) requires explicit human
    /// approval. Persisted in `.attini/{NAME}/plan_mode`.
    pub plan_mode: bool,
    /// DeepSeek thinking-mode effort for this session. `None` (= off)
    /// disables chain-of-thought so no `reasoning_content` is
    /// generated; the default. Persisted in
    /// `.attini/{NAME}/thinking_effort` as `none|low|high|max`.
    pub thinking_effort: ThinkingEffort,
}

impl Session {
    /// Open (or create) the session directory for `name` under
    /// `.attini/` in the current working directory. Takes the LOCK
    /// via `O_EXCL` and writes the holder's PID / start time. If the
    /// LOCK is stale (corrupted or holder dead), one retry is
    /// attempted; otherwise returns `Err(AlreadyExists)` with a hint.
    pub fn open(name: &str) -> io::Result<Self> {
        let paths = session_paths(name)?;
        fs::create_dir_all(&paths.dir)?;
        // Layer 2 free-write zone. Errors here surface as Session::open
        // failure: scratchpad is a hard prerequisite of the patch tool's
        // always-allow rule, so `warn + skip` would leave the invariant
        // silently broken.
        fs::create_dir_all(&paths.scratchpad)?;
        let lock = acquire_lock_with_stale_retry(name, &paths.lock)?;
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.conversation)?;
        let plan_mode = load_plan_mode(&paths.dir)?;
        let thinking_effort = load_thinking_effort(&paths.dir)?;
        Ok(Self {
            dir: paths.dir,
            lock_path: paths.lock,
            _lock: lock,
            conversation_path: paths.conversation,
            pending_path: paths.pending,
            writer,
            plan_mode,
            thinking_effort,
        })
    }

    /// Release the lock and close file handles. Called from
    /// `Drop`, but callers can invoke explicitly to surface I/O
    /// errors from unlink.
    pub fn close(self) -> io::Result<()> {
        fs::remove_file(&self.lock_path)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Load every `ChatMessage` recorded in `conversation.jsonl`
    /// so far, in order. Unknown record kinds are skipped
    /// (forward-compat). Malformed lines abort the load with an
    /// error since a corrupt history is not safely recoverable.
    ///
    /// Note: this loader ignores any `summary` records. Callers that
    /// need compaction-aware loading should use
    /// [`Session::load_summaries`] plus
    /// [`Session::load_records_since_last_summary`] and combine the
    /// two themselves.
    pub fn load_conversation(&self) -> io::Result<Vec<ChatMessage>> {
        let file = match File::open(&self.conversation_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut messages = Vec::new();
        for (i, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_conversation_line(&line) {
                Ok(Some(msg)) => messages.push(msg),
                Ok(None) => {}
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "malformed conversation record at line {}: {e}",
                        i + 1
                    )));
                }
            }
        }
        Ok(messages)
    }

    /// Return every summary text ever written to
    /// `conversation.jsonl`, in time order. Callers concatenate
    /// these as system messages before real records. Missing file
    /// yields an empty vector.
    pub fn load_summaries(&self) -> io::Result<Vec<SummaryRecord>> {
        let file = match File::open(&self.conversation_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut summaries = Vec::new();
        for (i, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_summary_line(&line) {
                Ok(Some(s)) => summaries.push(s),
                Ok(None) => {}
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "malformed conversation record at line {}: {e}",
                        i + 1
                    )));
                }
            }
        }
        Ok(summaries)
    }

    /// Return the real records (`user` / `assistant` / `tool`)
    /// whose `ts` is greater than the `cutoff_ts` of the newest
    /// summary. When there is no summary, every real record is
    /// returned.
    pub fn load_records_since_last_summary(&self) -> io::Result<Vec<ChatMessageWithTs>> {
        read_chat_message_with_ts(&self.conversation_path, false)
    }

    /// Latest `prompt_tokens` value recorded in `token_usage`
    /// records so far. Used by the compaction trigger to decide
    /// whether to summarise before the next invocation.
    pub fn latest_prompt_tokens(&self) -> io::Result<Option<u64>> {
        let file = match File::open(&self.conversation_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut latest: Option<u64> = None;
        for (i, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_prompt_tokens(&line) {
                Ok(Some(v)) => latest = Some(v),
                Ok(None) => {}
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "malformed conversation record at line {}: {e}",
                        i + 1
                    )));
                }
            }
        }
        Ok(latest)
    }

    pub fn conversation_path(&self) -> &Path {
        &self.conversation_path
    }

    pub fn pending_path(&self) -> &Path {
        &self.pending_path
    }

    /// Append a record to `conversation.jsonl` and flush.
    pub fn append(&mut self, record: &SessionRecord) -> io::Result<()> {
        let mut line = Json(record).to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }

    /// Write `pending.json` (overwriting any previous) as a JSON
    /// array of approval-blocked tool calls. The caller should
    /// follow up by exiting the process — the pending file signals
    /// to the next invocation that the agent loop is mid-turn.
    pub fn save_pending(&self, pending: &[Pending]) -> io::Result<()> {
        let content = Json(pending).to_string();
        fs::write(&self.pending_path, content)
    }

    /// Read `pending.json` if it exists. Returns the parked batch in
    /// array order (the order the tool calls appeared in the turn).
    pub fn load_pending(&self) -> io::Result<Option<Vec<Pending>>> {
        let text = match fs::read_to_string(&self.pending_path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let json =
            RawJson::parse(&text).map_err(|e| io::Error::other(format!("pending.json: {e}")))?;
        Pending::from_json_array(json.value()).map(Some)
    }

    /// Remove `pending.json` after a resume has been applied.
    pub fn clear_pending(&self) -> io::Result<()> {
        match fs::remove_file(&self.pending_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Set the session's plan-mode flag and persist it. `on=true`
    /// enables plan mode (every patch requires approval); `on=false`
    /// restores normal mode. Called when `--plan=on|off` is supplied;
    /// when the flag is omitted the persisted value is left unchanged.
    pub fn set_plan_mode(&mut self, on: bool) -> io::Result<()> {
        self.plan_mode = on;
        save_plan_mode(&self.dir, on)
    }

    /// Set the session's thinking-mode effort and persist it. Called
    /// when `--thinking-effort=...` is supplied; when the flag is
    /// omitted the persisted value is left unchanged.
    pub fn set_thinking_effort(&mut self, effort: ThinkingEffort) -> io::Result<()> {
        self.thinking_effort = effort;
        save_thinking_effort(&self.dir, effort)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort unlock. Explicit `close()` returns errors;
        // Drop swallows them.
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Read conversation records as [`ChatMessageWithTs`] from a path
/// without acquiring the session LOCK. When `all` is false, only
/// records newer than the newest summary's `cutoff_ts` are returned
/// (matching [`Session::load_records_since_last_summary`]); when true,
/// every real record is returned, ignoring summaries.
pub fn read_chat_message_with_ts(path: &Path, all: bool) -> io::Result<Vec<ChatMessageWithTs>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut latest_cutoff: Option<u64> = None;
    let mut records: Vec<ChatMessageWithTs> = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match parse_conversation_line_with_meta(&line) {
            Ok(LineKind::Message { message, ts }) => {
                records.push(ChatMessageWithTs { message, ts });
            }
            Ok(LineKind::Summary { cutoff_ts }) => {
                latest_cutoff = Some(match latest_cutoff {
                    Some(prev) => prev.max(cutoff_ts),
                    None => cutoff_ts,
                });
            }
            Ok(LineKind::Other) => {}
            Err(e) => {
                return Err(io::Error::other(format!(
                    "malformed conversation record at line {}: {e}",
                    i + 1
                )));
            }
        }
    }
    if !all && let Some(cutoff) = latest_cutoff {
        records.retain(|r| r.ts > cutoff);
    }
    Ok(records)
}

// -------------------------------------------------------------------
// Path resolution (LOCK not acquired, directory not created)
// -------------------------------------------------------------------

/// Absolute paths for a session's on-disk artifacts. Computed
/// without touching the filesystem so read-only commands can use
/// these without side effects.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub dir: PathBuf,
    pub conversation: PathBuf,
    pub pending: PathBuf,
    pub lock: PathBuf,
    /// Agent-writable free zone (`.attini/{NAME}/scratchpad/`). The
    /// patch tool's Layer 2 always-allow rule covers everything under
    /// this directory regardless of git status; `Session::open`
    /// auto-creates it so the agent can rely on its existence.
    pub scratchpad: PathBuf,
    /// Cached Q&A state for `attini ask` (`.attini/{NAME}/ask.json`).
    /// Not part of the conversation; consumed only by the read-only
    /// `ask` command to give follow-up questions prior context.
    pub ask: PathBuf,
    /// Persisted plan-mode flag (`.attini/{NAME}/plan_mode`). `1` means
    /// plan mode is on (every patch requires approval); `0` means normal.
    pub plan_mode: PathBuf,
    /// Persisted thinking-mode effort (`.attini/{NAME}/thinking_effort`)
    /// holding `none|low|high|max`. `none` disables thinking mode;
    /// `low|high|max` enable it at that depth.
    pub thinking_effort: PathBuf,
}

/// Root directory (`.attini/`) that holds every session in the CWD.
pub fn session_root() -> PathBuf {
    PathBuf::from(".attini")
}

/// Validate `name` and return the paths for its session. Does not
/// create the directory nor take the LOCK.
pub fn session_paths(name: &str) -> io::Result<SessionPaths> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must not be empty",
        ));
    }
    if name.contains(|c: char| c == '/' || c == '\\' || c.is_control()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must not contain path separators or control characters",
        ));
    }
    let dir = session_root().join(name);
    Ok(SessionPaths {
        conversation: dir.join("conversation.jsonl"),
        pending: dir.join("pending.json"),
        lock: dir.join("LOCK"),
        scratchpad: dir.join("scratchpad"),
        ask: dir.join("ask.json"),
        plan_mode: dir.join(PLAN_MODE_FILE),
        thinking_effort: dir.join(THINKING_EFFORT_FILE),
        dir,
    })
}

/// Filename holding the persisted plan-mode flag (contents `1` or `0`).
const PLAN_MODE_FILE: &str = "plan_mode";

/// Filename holding the persisted thinking-mode effort (contents
/// `none|low|high|max`).
const THINKING_EFFORT_FILE: &str = "thinking_effort";

/// Read a session's persisted plan-mode flag. Missing file, or an
/// unreadable / invalid value, defaults to `false` (normal mode).
/// Used by [`Session::open`] and by the read-only `session show`
/// command, which must not take the LOCK.
pub fn load_plan_mode(dir: &Path) -> io::Result<bool> {
    let text = match fs::read_to_string(dir.join(PLAN_MODE_FILE)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(text.trim() == "1")
}

/// Persist a session's plan-mode flag. Writes `1` (on) or `0` (off)
/// to `plan_mode` atomically (tmp + rename) so a crash cannot leave
/// a partial file.
pub fn save_plan_mode(dir: &Path, on: bool) -> io::Result<()> {
    let path = dir.join(PLAN_MODE_FILE);
    let tmp = dir.join(format!("{PLAN_MODE_FILE}.tmp"));
    fs::write(&tmp, if on { "1" } else { "0" })?;
    fs::rename(&tmp, &path)
}

/// Read a session's persisted thinking-mode effort. Missing file, or
/// an unreadable / invalid value, defaults to [`ThinkingEffort::High`]
/// (thinking on, high depth). Used by [`Session::open`] and by the read-only
/// `session show` command, which must not take the LOCK.
pub fn load_thinking_effort(dir: &Path) -> io::Result<ThinkingEffort> {
    let text = match fs::read_to_string(dir.join(THINKING_EFFORT_FILE)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ThinkingEffort::High),
        Err(e) => return Err(e),
    };
    Ok(ThinkingEffort::parse(text.trim()).unwrap_or(ThinkingEffort::High))
}

/// Persist a session's thinking-mode effort. Writes `none|low|high|max`
/// to `thinking_effort` atomically (tmp + rename) so a crash cannot
/// leave a partial file.
pub fn save_thinking_effort(dir: &Path, effort: ThinkingEffort) -> io::Result<()> {
    let path = dir.join(THINKING_EFFORT_FILE);
    let tmp = dir.join(format!("{THINKING_EFFORT_FILE}.tmp"));
    fs::write(&tmp, effort.as_str())?;
    fs::rename(&tmp, &path)
}

// -------------------------------------------------------------------
// Read-only inspection helpers for the `attini session` subcommand
// -------------------------------------------------------------------

/// Aggregate counters over one `conversation.jsonl`. Used by both
/// `attini session list` (total_records + last_ts) and
/// `attini session show` (per-kind breakdown).
#[derive(Debug, Clone, Default)]
pub struct ConversationSummary {
    pub total_records: u64,
    pub last_ts: Option<u64>,
    pub last_kind: Option<String>,
    pub invocation_starts: u64,
    pub invocation_ends_completed: u64,
    pub invocation_ends_awaiting_approval: u64,
    pub invocation_ends_error: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub assistant_tool_calls_total: u64,
    pub tool_messages: u64,
    pub approvals_approve: u64,
    pub approvals_reject: u64,
    /// Subset of `approvals_approve` whose record has an
    /// `auto_decided_by` sidecar (rule-driven auto approval).
    pub approvals_auto_approve: u64,
    /// Subset of `approvals_reject` whose record has an
    /// `auto_decided_by` sidecar (rule-driven auto deny or
    /// plan-mode reject).
    pub approvals_auto_deny: u64,
    pub summaries: u64,
    pub last_prompt_tokens: Option<u64>,
    pub last_prompt_cache_hit_tokens: Option<u64>,
    pub last_prompt_cache_miss_tokens: Option<u64>,
}

/// Walk `conversation.jsonl` and produce a summary. Missing file →
/// zero-initialised summary. Malformed lines abort with an error.
pub fn scan_conversation(path: &Path) -> io::Result<ConversationSummary> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(ConversationSummary::default());
        }
        Err(e) => return Err(e),
    };
    let mut summary = ConversationSummary::default();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        summary.total_records += 1;
        classify_record(&line, &mut summary)
            .map_err(|e| io::Error::other(format!("malformed record at line {}: {e}", i + 1)))?;
    }
    Ok(summary)
}

fn classify_record(line: &str, out: &mut ConversationSummary) -> Result<(), String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    if let Ok(m) = value.to_member("ts")
        && let Some(v) = m.optional()
        && let Ok(ts) = v.try_into()
    {
        out.last_ts = Some(ts);
    }
    match kind.as_str() {
        "invocation_start" => out.invocation_starts += 1,
        "invocation_end" => {
            let reason = value
                .to_member("reason")
                .and_then(|m| m.required())
                .and_then(|m| m.to_unquoted_string_str())
                .map_err(|e| e.to_string())?;
            match reason.as_ref() {
                "completed" => out.invocation_ends_completed += 1,
                "awaiting_approval" => out.invocation_ends_awaiting_approval += 1,
                "error" => out.invocation_ends_error += 1,
                _ => {}
            }
        }
        "user" => out.user_messages += 1,
        "assistant" => {
            out.assistant_messages += 1;
            if let Ok(m) = value.to_member("tool_calls")
                && let Some(v) = m.optional()
                && let Ok(arr) = v.to_array()
            {
                out.assistant_tool_calls_total += arr.count() as u64;
            }
        }
        "tool" => out.tool_messages += 1,
        "summary" => out.summaries += 1,
        "token_usage" => {
            if let Ok(usage_m) = value.to_member("usage")
                && let Some(usage) = usage_m.optional()
            {
                out.last_prompt_tokens = usage
                    .to_member("prompt_tokens")
                    .ok()
                    .and_then(|m| m.optional())
                    .and_then(|v| v.try_into().ok())
                    .or(out.last_prompt_tokens);
                out.last_prompt_cache_hit_tokens = usage
                    .to_member("prompt_cache_hit_tokens")
                    .ok()
                    .and_then(|m| m.optional())
                    .and_then(|v| v.try_into().ok())
                    .or(out.last_prompt_cache_hit_tokens);
                out.last_prompt_cache_miss_tokens = usage
                    .to_member("prompt_cache_miss_tokens")
                    .ok()
                    .and_then(|m| m.optional())
                    .and_then(|v| v.try_into().ok())
                    .or(out.last_prompt_cache_miss_tokens);
            }
        }
        "tool_approval" => {
            let decision = value
                .to_member("decision")
                .and_then(|m| m.required())
                .and_then(|m| m.to_unquoted_string_str())
                .map_err(|e| e.to_string())?;
            let has_auto_sidecar = value
                .to_member("auto_decided_by")
                .ok()
                .and_then(|m| m.optional())
                .is_some();
            match decision.as_ref() {
                "approve" => {
                    out.approvals_approve += 1;
                    if has_auto_sidecar {
                        out.approvals_auto_approve += 1;
                    }
                }
                "reject" => {
                    out.approvals_reject += 1;
                    if has_auto_sidecar {
                        out.approvals_auto_deny += 1;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    out.last_kind = Some(kind);
    Ok(())
}

/// A subset of `Pending` safe to show to the user (omits the full
/// `arguments_json`, which for command tools would echo the entire
/// shell command line and for patches the full replacement text).
#[derive(Debug, Clone)]
pub struct PendingSummary {
    pub call_id: String,
    pub tool_kind: PendingToolKind,
    pub function_name: String,
    pub preview: String,
    pub ts: u64,
}

/// Read `pending.json` and return summaries of every parked call.
/// Missing file → `None`.
pub fn read_pending_summary(path: &Path) -> io::Result<Option<Vec<PendingSummary>>> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let json = RawJson::parse(&text).map_err(|e| io::Error::other(format!("pending.json: {e}")))?;
    let pendings = Pending::from_json_array(json.value())?;
    Ok(Some(
        pendings
            .into_iter()
            .map(|p| PendingSummary {
                call_id: p.call_id,
                tool_kind: p.tool_kind,
                function_name: p.function_name,
                preview: p.preview,
                ts: p.ts,
            })
            .collect(),
    ))
}

// -------------------------------------------------------------------
// LOCK acquisition (with stale detection)
// -------------------------------------------------------------------

fn acquire_lock_with_stale_retry(name: &str, lock_path: &Path) -> io::Result<File> {
    match acquire_lock(lock_path) {
        Ok(file) => Ok(file),
        Err(AcquireError::Io(e)) => Err(e),
        Err(AcquireError::Locked) => match inspect_lock(lock_path) {
            LockStatus::PidAlive(pid) => Err(lock_conflict_error(name, lock_path, Some(pid))),
            LockStatus::PidDead | LockStatus::Corrupted | LockStatus::None => {
                let _ = fs::remove_file(lock_path);
                match acquire_lock(lock_path) {
                    Ok(file) => Ok(file),
                    Err(AcquireError::Io(e)) => Err(e),
                    Err(AcquireError::Locked) => Err(lock_conflict_error(name, lock_path, None)),
                }
            }
        },
    }
}

enum AcquireError {
    Locked,
    Io(io::Error),
}

fn acquire_lock(path: &Path) -> Result<File, AcquireError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                AcquireError::Locked
            } else {
                AcquireError::Io(e)
            }
        })?;
    let body = LockBody {
        pid: std::process::id() as i32,
        started_at_unix_ms: now_unix_millis(),
    };
    let text = Json(&body).to_string();
    file.write_all(text.as_bytes()).map_err(AcquireError::Io)?;
    file.sync_all().map_err(AcquireError::Io)?;
    Ok(file)
}

fn lock_conflict_error(name: &str, lock_path: &Path, holder_pid: Option<i32>) -> io::Error {
    let pid_hint = match holder_pid {
        Some(pid) => format!(" (holder pid {pid})"),
        None => String::new(),
    };
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "session {name:?} is locked{pid_hint}: {path}\n\
             If no attini process is actually holding it, remove the LOCK manually: \
             `rm {path}` (or `attini session unlock {name}` once that command lands).",
            path = lock_path.display(),
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    /// No LOCK file present.
    None,
    /// LOCK file exists but its contents cannot be parsed / are
    /// empty / carry an invalid PID.
    Corrupted,
    /// LOCK file names a PID that no longer exists (`ESRCH`).
    PidDead,
    /// LOCK file names a PID that is alive, or that `kill(pid, 0)`
    /// reports as EPERM (safe side: treat as held).
    PidAlive(i32),
}

/// Non-mutating probe of a LOCK file. Does not create, open with
/// exclusive access, or otherwise disturb the file.
pub fn inspect_lock(path: &Path) -> LockStatus {
    match fs::metadata(path) {
        Ok(_) => classify_existing_lock(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => LockStatus::None,
        Err(_) => LockStatus::Corrupted,
    }
}

fn classify_existing_lock(path: &Path) -> LockStatus {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return LockStatus::Corrupted,
    };
    let (pid, _started_at) = match parse_lock_body(&text) {
        Some(v) => v,
        None => return LockStatus::Corrupted,
    };
    match probe_pid(pid) {
        PidStatus::Dead => LockStatus::PidDead,
        PidStatus::Alive | PidStatus::EPerm => LockStatus::PidAlive(pid),
    }
}

fn parse_lock_body(text: &str) -> Option<(i32, u64)> {
    let json = RawJson::parse(text).ok()?;
    let value = json.value();
    let pid_i64: i64 = value
        .to_member("pid")
        .ok()?
        .required()
        .ok()?
        .try_into()
        .ok()?;
    let started_at_unix_ms: u64 = value
        .to_member("started_at_unix_ms")
        .ok()?
        .required()
        .ok()?
        .try_into()
        .ok()?;
    let pid: i32 = pid_i64.try_into().ok()?;
    if pid <= 0 {
        return None;
    }
    Some((pid, started_at_unix_ms))
}

enum PidStatus {
    Alive,
    Dead,
    EPerm,
}

fn probe_pid(pid: i32) -> PidStatus {
    // SAFETY: `kill` with signal 0 does not send a signal; it only
    // probes whether the process (or one with the same effective
    // uid) exists. No memory safety concerns.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return PidStatus::Alive;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(errno) if errno == libc::ESRCH => PidStatus::Dead,
        Some(errno) if errno == libc::EPERM => PidStatus::EPerm,
        _ => PidStatus::Alive,
    }
}

struct LockBody {
    pid: i32,
    started_at_unix_ms: u64,
}

impl DisplayJson for LockBody {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("pid", self.pid)?;
            f.member("started_at_unix_ms", self.started_at_unix_ms)
        })
    }
}

// -------------------------------------------------------------------
// SessionRecord (on-disk record types for conversation.jsonl)
// -------------------------------------------------------------------

/// One line written to `conversation.jsonl`. Kept intentionally
/// small — only what the CLI shell needs to reconstruct state
/// and print a useful history. Not the same schema as the TUI's
/// `TranscriptRecord` (which is oriented at event observers);
/// having a dedicated CLI schema keeps the format easy to
/// parse and evolve independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRecord {
    InvocationStart {
        ts: u64,
        attini_version: String,
        model: String,
    },
    InvocationEnd {
        ts: u64,
        reason: InvocationEndReason,
    },
    User {
        ts: u64,
        text: String,
    },
    Assistant {
        ts: u64,
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        ts: u64,
        call_id: String,
        content: String,
    },
    ToolApproval {
        ts: u64,
        call_id: String,
        decision: ApprovalDecision,
        /// `Some` when the decision was made automatically (rule
        /// match or plan-mode reject); `None` for user `--approve`
        /// / `--reject`. Serialised as an optional sidecar object.
        auto_decided_by: Option<AutoDecidedBy>,
    },
    /// Snapshot of transport / agent metric counters. Emitted
    /// once at invocation end.
    MetricsSnapshot {
        ts: u64,
        counters: MetricsSnapshotBody,
    },
    /// Per-turn token usage reported by the model. Appended after
    /// each successful assistant turn when the streaming response
    /// carried a `usage` object. Compaction reads the latest
    /// `prompt_tokens` from these records to decide whether to
    /// summarise before the next invocation.
    TokenUsage {
        ts: u64,
        body: TokenUsageBody,
    },
    /// A compaction summary that replaces the range of real records
    /// from `since_ts` up to and including `cutoff_ts`. Multiple
    /// summaries accumulate in time order; the shell reads them all
    /// and only the real records after the latest `cutoff_ts`.
    Summary {
        ts: u64,
        since_ts: u64,
        cutoff_ts: u64,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationEndReason {
    /// Turn completed with a final assistant message (no pending
    /// tool calls).
    Completed,
    /// Agent loop suspended waiting for approval. `pending.json`
    /// is populated.
    AwaitingApproval,
    /// Something errored before completion.
    Error,
    /// The invocation-scope tool-call backstop
    /// (`AgentConfig::session_tool_call_max`) tripped and the loop
    /// stopped without a final assistant message.
    SessionToolCallExhausted,
}

impl InvocationEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Error => "error",
            Self::SessionToolCallExhausted => "session_tool_call_exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
}

impl ApprovalDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }
}

/// Sidecar attached to `SessionRecord::ToolApproval` when the
/// decision was made automatically (rule match, planning-mode reject,
/// or an approved plan action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDecidedBy {
    pub scope: String,
    pub argv_prefix: Vec<String>,
    pub reason: String,
}

impl DisplayJson for AutoDecidedBy {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("scope", &self.scope)?;
            f.member("argv_prefix", &self.argv_prefix)?;
            f.member("reason", &self.reason)?;
            Ok(())
        })
    }
}

/// Placeholder body for a metrics snapshot. Filled with a flat
/// map of counter name → value so the shape can be inspected by
/// `jq` without a schema.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetricsSnapshotBody {
    pub entries: Vec<(String, u64)>,
}

impl DisplayJson for MetricsSnapshotBody {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            for (k, v) in &self.entries {
                f.member(k.as_str(), v)?;
            }
            Ok(())
        })
    }
}

/// Per-turn token usage payload for `SessionRecord::TokenUsage`.
/// All fields are optional because different models populate
/// different subsets; only `prompt_tokens` is required for
/// compaction to trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsageBody {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub prompt_cache_hit_tokens: Option<u64>,
    pub prompt_cache_miss_tokens: Option<u64>,
}

impl DisplayJson for TokenUsageBody {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            if let Some(v) = self.prompt_tokens {
                f.member("prompt_tokens", v)?;
            }
            if let Some(v) = self.completion_tokens {
                f.member("completion_tokens", v)?;
            }
            if let Some(v) = self.total_tokens {
                f.member("total_tokens", v)?;
            }
            if let Some(v) = self.prompt_cache_hit_tokens {
                f.member("prompt_cache_hit_tokens", v)?;
            }
            if let Some(v) = self.prompt_cache_miss_tokens {
                f.member("prompt_cache_miss_tokens", v)?;
            }
            Ok(())
        })
    }
}

impl DisplayJson for SessionRecord {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        match self {
            Self::InvocationStart {
                ts,
                attini_version,
                model,
            } => f.object(|f| {
                f.member("kind", "invocation_start")?;
                f.member("ts", ts)?;
                f.member("attini_version", attini_version)?;
                f.member("model", model)
            }),
            Self::InvocationEnd { ts, reason } => f.object(|f| {
                f.member("kind", "invocation_end")?;
                f.member("ts", ts)?;
                f.member("reason", reason.as_str())
            }),
            Self::User { ts, text } => f.object(|f| {
                f.member("kind", "user")?;
                f.member("ts", ts)?;
                f.member("text", text)
            }),
            Self::Assistant {
                ts,
                content,
                reasoning,
                tool_calls,
            } => f.object(|f| {
                f.member("kind", "assistant")?;
                f.member("ts", ts)?;
                f.member("content", content)?;
                f.member("reasoning", reasoning)?;
                f.member("tool_calls", tool_calls)
            }),
            Self::Tool {
                ts,
                call_id,
                content,
            } => f.object(|f| {
                f.member("kind", "tool")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("content", content)
            }),
            Self::ToolApproval {
                ts,
                call_id,
                decision,
                auto_decided_by,
            } => f.object(|f| {
                f.member("kind", "tool_approval")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("decision", decision.as_str())?;
                if let Some(by) = auto_decided_by {
                    f.member("auto_decided_by", by)?;
                }
                Ok(())
            }),
            Self::MetricsSnapshot { ts, counters } => f.object(|f| {
                f.member("kind", "metrics_snapshot")?;
                f.member("ts", ts)?;
                f.member("counters", counters)
            }),
            Self::TokenUsage { ts, body } => f.object(|f| {
                f.member("kind", "token_usage")?;
                f.member("ts", ts)?;
                f.member("usage", body)
            }),
            Self::Summary {
                ts,
                since_ts,
                cutoff_ts,
                text,
            } => f.object(|f| {
                f.member("kind", "summary")?;
                f.member("ts", ts)?;
                f.member("since_ts", since_ts)?;
                f.member("cutoff_ts", cutoff_ts)?;
                f.member("text", text)
            }),
        }
    }
}

/// One `summary` record from `conversation.jsonl`, with the
/// metadata compaction and inspection code needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRecord {
    pub ts: u64,
    pub since_ts: u64,
    pub cutoff_ts: u64,
    pub text: String,
}

/// A conversation `ChatMessage` paired with the `ts` of the record
/// it came from. Used by compaction to decide safe cutoff
/// boundaries and to compute `since_ts` for a new summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageWithTs {
    pub message: ChatMessage,
    pub ts: u64,
}

enum LineKind {
    Message { message: ChatMessage, ts: u64 },
    Summary { cutoff_ts: u64 },
    Other,
}

fn parse_conversation_line_with_meta(line: &str) -> Result<LineKind, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    match kind.as_str() {
        "user" | "assistant" | "tool" => {
            let ts: u64 = value
                .to_member("ts")
                .and_then(|m| m.required())
                .and_then(|m| m.try_into())
                .map_err(|e| e.to_string())?;
            let message = parse_conversation_line(line)?.ok_or_else(|| {
                "kind matched user/assistant/tool but parse_conversation_line returned None"
                    .to_string()
            })?;
            Ok(LineKind::Message { message, ts })
        }
        "summary" => {
            let cutoff_ts: u64 = value
                .to_member("cutoff_ts")
                .and_then(|m| m.required())
                .and_then(|m| m.try_into())
                .map_err(|e| e.to_string())?;
            Ok(LineKind::Summary { cutoff_ts })
        }
        _ => Ok(LineKind::Other),
    }
}

fn parse_summary_line(line: &str) -> Result<Option<SummaryRecord>, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    if kind != "summary" {
        return Ok(None);
    }
    let ts: u64 = value
        .to_member("ts")
        .and_then(|m| m.required())
        .and_then(|m| m.try_into())
        .map_err(|e| e.to_string())?;
    let since_ts: u64 = value
        .to_member("since_ts")
        .and_then(|m| m.required())
        .and_then(|m| m.try_into())
        .map_err(|e| e.to_string())?;
    let cutoff_ts: u64 = value
        .to_member("cutoff_ts")
        .and_then(|m| m.required())
        .and_then(|m| m.try_into())
        .map_err(|e| e.to_string())?;
    let text = read_string(value, "text")?;
    Ok(Some(SummaryRecord {
        ts,
        since_ts,
        cutoff_ts,
        text,
    }))
}

fn parse_prompt_tokens(line: &str) -> Result<Option<u64>, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    if kind != "token_usage" {
        return Ok(None);
    }
    let Some(usage) = value
        .to_member("usage")
        .map_err(|e| e.to_string())?
        .optional()
    else {
        return Ok(None);
    };
    let Some(pt) = usage
        .to_member("prompt_tokens")
        .map_err(|e| e.to_string())?
        .optional()
    else {
        return Ok(None);
    };
    let n: u64 = pt.try_into().map_err(|e: JsonParseError| e.to_string())?;
    Ok(Some(n))
}

/// Read every `SessionRecord` from a `conversation.jsonl` file
/// without acquiring the session LOCK. Suited for observing a
/// session's tail while it (or another process) still holds the
/// LOCK. Missing files return an empty vector. Individual malformed
/// or unknown lines are silently skipped so a partial file (e.g. a
/// half-written last line from a crashed writer) still yields the
/// prefix of well-formed records.
pub fn read_conversation_records(path: &Path) -> io::Result<Vec<SessionRecord>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let reader = io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(Some(record)) = parse_session_record_line(&line) {
            out.push(record);
        }
    }
    Ok(out)
}

/// Deserialize one JSON Lines record into the corresponding
/// [`SessionRecord`] variant. Unknown `kind` values return
/// `Ok(None)` so callers can iterate a mixed stream without
/// erroring on forward-compatible additions. Malformed JSON (missing
/// required field, wrong type) returns `Err`.
fn parse_session_record_line(line: &str) -> Result<Option<SessionRecord>, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    match kind.as_str() {
        "invocation_start" => {
            let ts = read_u64(value, "ts")?;
            let attini_version = read_string(value, "attini_version")?;
            let model = read_string(value, "model")?;
            Ok(Some(SessionRecord::InvocationStart {
                ts,
                attini_version,
                model,
            }))
        }
        "invocation_end" => {
            let ts = read_u64(value, "ts")?;
            let reason_str = read_string(value, "reason")?;
            let reason = match reason_str.as_str() {
                "completed" => InvocationEndReason::Completed,
                "awaiting_approval" => InvocationEndReason::AwaitingApproval,
                "error" => InvocationEndReason::Error,
                "session_tool_call_exhausted" => InvocationEndReason::SessionToolCallExhausted,
                other => return Err(format!("unknown invocation_end.reason {other:?}")),
            };
            Ok(Some(SessionRecord::InvocationEnd { ts, reason }))
        }
        "user" => {
            let ts = read_u64(value, "ts")?;
            let text = read_string(value, "text")?;
            Ok(Some(SessionRecord::User { ts, text }))
        }
        "assistant" => {
            let ts = read_u64(value, "ts")?;
            let content = read_string(value, "content")?;
            let reasoning = read_optional_string(value, "reasoning")?;
            let tool_calls = read_tool_calls(value)?;
            Ok(Some(SessionRecord::Assistant {
                ts,
                content,
                reasoning,
                tool_calls,
            }))
        }
        "tool" => {
            let ts = read_u64(value, "ts")?;
            let call_id = read_string(value, "call_id")?;
            let content = read_string(value, "content")?;
            Ok(Some(SessionRecord::Tool {
                ts,
                call_id,
                content,
            }))
        }
        "tool_approval" => {
            let ts = read_u64(value, "ts")?;
            let call_id = read_string(value, "call_id")?;
            let decision_str = read_string(value, "decision")?;
            let decision = match decision_str.as_str() {
                "approve" => ApprovalDecision::Approve,
                "reject" => ApprovalDecision::Reject,
                other => return Err(format!("unknown tool_approval.decision {other:?}")),
            };
            let auto_decided_by = parse_auto_decided_by(value)?;
            Ok(Some(SessionRecord::ToolApproval {
                ts,
                call_id,
                decision,
                auto_decided_by,
            }))
        }
        "metrics_snapshot" => {
            let ts = read_u64(value, "ts")?;
            let counters = parse_metrics_counters(value)?;
            Ok(Some(SessionRecord::MetricsSnapshot {
                ts,
                counters: MetricsSnapshotBody { entries: counters },
            }))
        }
        "token_usage" => {
            let ts = read_u64(value, "ts")?;
            let body = parse_token_usage_body(value)?;
            Ok(Some(SessionRecord::TokenUsage { ts, body }))
        }
        "summary" => {
            let ts = read_u64(value, "ts")?;
            let since_ts = read_u64(value, "since_ts")?;
            let cutoff_ts = read_u64(value, "cutoff_ts")?;
            let text = read_string(value, "text")?;
            Ok(Some(SessionRecord::Summary {
                ts,
                since_ts,
                cutoff_ts,
                text,
            }))
        }
        _ => Ok(None),
    }
}

fn read_u64(value: nojson::RawJsonValue<'_, '_>, key: &str) -> Result<u64, String> {
    value
        .to_member(key)
        .and_then(|m| m.required())
        .and_then(|m| m.try_into())
        .map_err(|e: JsonParseError| e.to_string())
}

fn parse_auto_decided_by(
    value: nojson::RawJsonValue<'_, '_>,
) -> Result<Option<AutoDecidedBy>, String> {
    let Some(m) = value
        .to_member("auto_decided_by")
        .map_err(|e| e.to_string())?
        .optional()
    else {
        return Ok(None);
    };
    if m.as_raw_str().trim() == "null" {
        return Ok(None);
    }
    let scope = read_string(m, "scope")?;
    let reason = read_string(m, "reason")?;
    let mut argv_prefix = Vec::new();
    let list = m
        .to_member("argv_prefix")
        .and_then(|arr| arr.required())
        .map_err(|e| e.to_string())?;
    for item in list.to_array().map_err(|e| e.to_string())? {
        argv_prefix.push(
            item.to_unquoted_string_str()
                .map_err(|e| e.to_string())?
                .into_owned(),
        );
    }
    Ok(Some(AutoDecidedBy {
        scope,
        argv_prefix,
        reason,
    }))
}

fn parse_metrics_counters(
    value: nojson::RawJsonValue<'_, '_>,
) -> Result<Vec<(String, u64)>, String> {
    let counters = value
        .to_member("counters")
        .and_then(|m| m.required())
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (k, v) in counters.to_object().map_err(|e| e.to_string())? {
        let name = k
            .to_unquoted_string_str()
            .map_err(|e| e.to_string())?
            .into_owned();
        let n: u64 = v.try_into().map_err(|e: JsonParseError| e.to_string())?;
        out.push((name, n));
    }
    Ok(out)
}

fn parse_token_usage_body(value: nojson::RawJsonValue<'_, '_>) -> Result<TokenUsageBody, String> {
    let usage = value
        .to_member("usage")
        .and_then(|m| m.required())
        .map_err(|e| e.to_string())?;
    let opt_u64 = |key: &str| -> Result<Option<u64>, String> {
        let Some(v) = usage.to_member(key).map_err(|e| e.to_string())?.optional() else {
            return Ok(None);
        };
        if v.as_raw_str().trim() == "null" {
            return Ok(None);
        }
        let n: u64 = v.try_into().map_err(|e: JsonParseError| e.to_string())?;
        Ok(Some(n))
    };
    Ok(TokenUsageBody {
        prompt_tokens: opt_u64("prompt_tokens")?,
        completion_tokens: opt_u64("completion_tokens")?,
        total_tokens: opt_u64("total_tokens")?,
        prompt_cache_hit_tokens: opt_u64("prompt_cache_hit_tokens")?,
        prompt_cache_miss_tokens: opt_u64("prompt_cache_miss_tokens")?,
    })
}

/// Extract a [`ChatMessage`] from one JSON Lines record if the
/// record contributes to the conversation context. Returns
/// `Ok(None)` for records that are logged for observability but
/// do not add to the message list (invocation_start /
/// invocation_end / tool_approval / metrics_snapshot).
fn parse_conversation_line(line: &str) -> Result<Option<ChatMessage>, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let kind = value
        .to_member("kind")
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned();
    match kind.as_str() {
        "user" => {
            let text = read_string(value, "text")?;
            Ok(Some(ChatMessage::User(text)))
        }
        "assistant" => {
            let content = read_string(value, "content")?;
            let reasoning = read_optional_string(value, "reasoning")?;
            let tool_calls = read_tool_calls(value)?;
            // Reasoning is the model's private CoT trace. It is persisted
            // for observability but is not replayed into later invocations:
            // re-sending it bloats every request and can anchor the model
            // to stale thinking. The lone exception is a turn that carried
            // its answer entirely in `reasoning` (blank content, no tool
            // calls); promote that to content so the answer is not lost.
            let (content, reasoning_content) = match (content, reasoning) {
                (c, Some(r)) if c.is_empty() && tool_calls.is_empty() => (r, None),
                (c, _) => (c, None),
            };
            Ok(Some(ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            }))
        }
        "tool" => {
            let call_id = read_string(value, "call_id")?;
            let content = read_string(value, "content")?;
            Ok(Some(ChatMessage::Tool {
                tool_call_id: call_id,
                content,
            }))
        }
        // Non-conversation records (start/end/approval/metrics)
        // are recorded for observability but do not add to the
        // message list. Unknown kinds are skipped forward-compat.
        _ => Ok(None),
    }
}

fn read_string(value: nojson::RawJsonValue<'_, '_>, key: &str) -> Result<String, String> {
    Ok(value
        .to_member(key)
        .and_then(|m| m.required())
        .and_then(|m| m.to_unquoted_string_str())
        .map_err(|e| e.to_string())?
        .into_owned())
}

fn read_optional_string(
    value: nojson::RawJsonValue<'_, '_>,
    key: &str,
) -> Result<Option<String>, String> {
    let member = value.to_member(key).map_err(|e| e.to_string())?;
    let Some(v) = member.optional() else {
        return Ok(None);
    };
    // Distinguish JSON `null` from a real string.
    let is_null = v.as_raw_str().trim() == "null";
    if is_null {
        return Ok(None);
    }
    Ok(Some(
        v.to_unquoted_string_str()
            .map_err(|e| e.to_string())?
            .into_owned(),
    ))
}

fn read_tool_calls(value: nojson::RawJsonValue<'_, '_>) -> Result<Vec<ToolCall>, String> {
    let member = value.to_member("tool_calls").map_err(|e| e.to_string())?;
    let Some(v) = member.optional() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in v.to_array().map_err(|e| e.to_string())? {
        let id = read_string(item, "id")?;
        let function = item
            .to_member("function")
            .and_then(|m| m.required())
            .map_err(|e| e.to_string())?;
        let function_name = read_string(function, "name")?;
        let arguments_json = read_string(function, "arguments")?;
        out.push(ToolCall {
            id,
            function_name,
            arguments_json,
        });
    }
    Ok(out)
}

// -------------------------------------------------------------------
// Pending state (pending.json)
// -------------------------------------------------------------------

/// Serialisable snapshot of one approval-blocked point in the
/// agent loop. The next invocation loads this to know which tool
/// call to execute (on approve) or synthesise a rejection for
/// (on reject).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub ts: u64,
    pub call_id: String,
    pub tool_kind: PendingToolKind,
    pub function_name: String,
    pub arguments_json: String,
    /// Human-readable preview shown to the user (already printed
    /// during the invocation that produced this pending record).
    /// Kept here so the resuming invocation can re-print if the
    /// user forgot.
    pub preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingToolKind {
    Patch,
    Command,
}

impl PendingToolKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Command => "command",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "patch" => Some(Self::Patch),
            "command" => Some(Self::Command),
            _ => None,
        }
    }
}

impl DisplayJson for Pending {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("ts", self.ts)?;
            f.member("call_id", &self.call_id)?;
            f.member("tool_kind", self.tool_kind.as_str())?;
            f.member("function_name", &self.function_name)?;
            f.member("arguments_json", &self.arguments_json)?;
            f.member("preview", &self.preview)
        })
    }
}

impl Pending {
    fn from_json(value: nojson::RawJsonValue<'_, '_>) -> io::Result<Self> {
        let map_err = |e: String| io::Error::other(format!("pending.json: {e}"));
        let ts: u64 = value
            .to_member("ts")
            .and_then(|m| m.required())
            .and_then(|m| m.try_into())
            .map_err(|e| map_err(e.to_string()))?;
        let call_id = read_string(value, "call_id").map_err(map_err)?;
        let tool_kind_str = read_string(value, "tool_kind").map_err(map_err)?;
        let tool_kind = PendingToolKind::parse(&tool_kind_str).ok_or_else(|| {
            io::Error::other(format!("pending.json: unknown tool_kind {tool_kind_str:?}"))
        })?;
        let function_name = read_string(value, "function_name").map_err(map_err)?;
        let arguments_json = read_string(value, "arguments_json").map_err(map_err)?;
        let preview = read_string(value, "preview").map_err(map_err)?;
        Ok(Self {
            ts,
            call_id,
            tool_kind,
            function_name,
            arguments_json,
            preview,
        })
    }

    fn from_json_array(value: nojson::RawJsonValue<'_, '_>) -> io::Result<Vec<Self>> {
        let iter = value
            .to_array()
            .map_err(|e| io::Error::other(format!("pending.json: {e}")))?;
        let mut out = Vec::new();
        for elem in iter {
            out.push(Self::from_json(elem)?);
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------
// ask state (ask.json)
// -------------------------------------------------------------------

/// One cached Q&A from `attini ask`. Purely advisory: it is never
/// injected into the agent's conversation, only passed to a later
/// `attini ask` so a follow-up question can build on a previous
/// observer answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskEntry {
    pub ts: u64,
    pub question: Option<String>,
    pub answer: String,
}

/// Snapshot of the question/answer cache for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskState {
    /// FNV-1a fingerprint of the conversation records that produced
    /// the first entry in this state. `ask` recomputes it for the
    /// window it actually observed; a mismatch means the observation
    /// changed (session advanced, or `--all`/`--limit` differs) and
    /// the cache is discarded rather than trusted.
    pub conversation_fingerprint: u64,
    pub entries: Vec<AskEntry>,
}

impl DisplayJson for AskEntry {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("ts", self.ts)?;
            f.member("question", &self.question)?;
            f.member("answer", &self.answer)
        })
    }
}

impl DisplayJson for AskState {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("conversation_fingerprint", self.conversation_fingerprint)?;
            f.member("entries", &self.entries)
        })
    }
}

impl AskState {
    fn from_json(value: nojson::RawJsonValue<'_, '_>) -> io::Result<Self> {
        let map_err = |e: String| io::Error::other(format!("ask.json: {e}"));
        let conversation_fingerprint: u64 = value
            .to_member("conversation_fingerprint")
            .and_then(|m| m.required())
            .and_then(|m| m.try_into())
            .map_err(|e| map_err(e.to_string()))?;
        let iter = value
            .to_member("entries")
            .and_then(|m| m.required())
            .and_then(|m| m.to_array())
            .map_err(|e| map_err(e.to_string()))?;
        let mut entries = Vec::new();
        for elem in iter {
            entries.push(Self::entry_from_json(elem).map_err(&map_err)?);
        }
        Ok(Self {
            conversation_fingerprint,
            entries,
        })
    }

    fn entry_from_json(value: nojson::RawJsonValue<'_, '_>) -> Result<AskEntry, String> {
        let ts: u64 = value
            .to_member("ts")
            .and_then(|m| m.required())
            .and_then(|m| m.try_into())
            .map_err(|e| e.to_string())?;
        let question = read_optional_string(value, "question")?;
        let answer = read_string(value, "answer")?;
        Ok(AskEntry {
            ts,
            question,
            answer,
        })
    }
}

/// Read `ask.json` for a session if present. A missing file yields
/// `Ok(None)`; malformed content is a hard error (it would silently
/// break follow-up context otherwise).
pub fn load_ask_state(path: &Path) -> io::Result<Option<AskState>> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let json = RawJson::parse(&text).map_err(|e| io::Error::other(format!("ask.json: {e}")))?;
    AskState::from_json(json.value()).map(Some)
}

/// Write `ask.json` atomically (temp file in the same directory,
/// then rename) so a concurrent reader never sees a partial file.
pub fn save_ask_state(path: &Path, state: &AskState) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ask.json has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ask.json");
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::write(&tmp, Json(state).to_string())?;
    fs::rename(&tmp, path)
}

/// FNV-1a (64-bit) of the ordered conversation records. Deterministic
/// across processes (unlike `DefaultHasher`), and only depends on the
/// records actually observed — so a different `--all`/`--limit` window
/// produces a different fingerprint and naturally resets the ask cache.
pub fn conversation_fingerprint(records: &[ChatMessageWithTs]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for rec in records {
        for b in rec.ts.to_string().as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xFF;
        hash = hash.wrapping_mul(PRIME);
        let body = Json(&rec.message).to_string();
        for b in body.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xFE;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// -------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------

pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_fingerprint_is_deterministic_and_window_sensitive() {
        let a = ChatMessageWithTs {
            message: ChatMessage::User("hello".to_string()),
            ts: 1,
        };
        let b = ChatMessageWithTs {
            message: ChatMessage::User("world".to_string()),
            ts: 2,
        };
        let fp1 = conversation_fingerprint(&[a.clone(), b.clone()]);
        let fp2 = conversation_fingerprint(&[a.clone(), b.clone()]);
        assert_eq!(fp1, fp2, "fingerprint must be deterministic");

        let fp_more = conversation_fingerprint(&[a.clone(), b.clone(), a.clone()]);
        assert_ne!(fp1, fp_more, "adding a record must change the fingerprint");

        let fp_window = conversation_fingerprint(&[b]);
        assert_ne!(
            fp1, fp_window,
            "a different window must change the fingerprint"
        );
    }

    #[test]
    fn ask_state_roundtrips_through_json() {
        let state = AskState {
            conversation_fingerprint: 42,
            entries: vec![
                AskEntry {
                    ts: 1,
                    question: Some("what?".to_string()),
                    answer: "answer one".to_string(),
                },
                AskEntry {
                    ts: 2,
                    question: None,
                    answer: "answer two".to_string(),
                },
            ],
        };
        let json = Json(&state).to_string();
        let parsed = RawJson::parse(&json).expect("ask.json should parse");
        let got = AskState::from_json(parsed.value()).expect("ask.json should convert");
        assert_eq!(got, state);
    }

    #[test]
    fn approval_decision_serialises_stable_strings() {
        assert_eq!(ApprovalDecision::Approve.as_str(), "approve");
        assert_eq!(ApprovalDecision::Reject.as_str(), "reject");
    }

    #[test]
    fn invocation_end_reason_serialises_stable_strings() {
        assert_eq!(InvocationEndReason::Completed.as_str(), "completed");
        assert_eq!(
            InvocationEndReason::AwaitingApproval.as_str(),
            "awaiting_approval"
        );
        assert_eq!(InvocationEndReason::Error.as_str(), "error");
        assert_eq!(
            InvocationEndReason::SessionToolCallExhausted.as_str(),
            "session_tool_call_exhausted"
        );
    }

    #[test]
    fn pending_tool_kind_roundtrips() {
        for kind in [PendingToolKind::Patch, PendingToolKind::Command] {
            assert_eq!(PendingToolKind::parse(kind.as_str()), Some(kind));
        }
        assert!(PendingToolKind::parse("bogus").is_none());
    }

    #[test]
    fn pending_batch_roundtrips_through_json_array() {
        let a = Pending {
            ts: 1,
            call_id: "call_1".to_string(),
            tool_kind: PendingToolKind::Patch,
            function_name: "patch".to_string(),
            arguments_json: r#"{"edits":[]}"#.to_string(),
            preview: "patch preview".to_string(),
        };
        let b = Pending {
            ts: 2,
            call_id: "call_2".to_string(),
            tool_kind: PendingToolKind::Command,
            function_name: "command".to_string(),
            arguments_json: r#"{"argv":["git","status"]}"#.to_string(),
            preview: "command preview".to_string(),
        };
        let json = Json(&[a.clone(), b.clone()]).to_string();
        let parsed = RawJson::parse(&json).expect("array should parse");
        let got = Pending::from_json_array(parsed.value()).expect("array should convert");
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn assistant_record_with_tool_calls_roundtrips_through_parse() {
        let record = SessionRecord::Assistant {
            ts: 42,
            content: "hi".to_string(),
            reasoning: Some("because".to_string()),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                function_name: "read".to_string(),
                arguments_json: r#"{"path":"src/foo.rs"}"#.to_string(),
            }],
        };
        let line = nojson::Json(&record).to_string();
        let parsed = parse_conversation_line(&line)
            .expect("parse must succeed")
            .expect("assistant record must yield a ChatMessage");
        match parsed {
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(content, "hi");
                // Reasoning is stripped on replay when content or tool_calls
                // already carry the turn; it is not re-sent to the model.
                assert_eq!(reasoning_content.as_deref(), None);
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "call_1");
                assert_eq!(tool_calls[0].function_name, "read");
                assert_eq!(tool_calls[0].arguments_json, r#"{"path":"src/foo.rs"}"#);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn assistant_reasoning_promoted_when_it_carries_the_answer() {
        let record = SessionRecord::Assistant {
            ts: 1,
            // Some reasoning-capable models answer in `reasoning` while
            // leaving `content` blank; the answer must not be lost.
            content: String::new(),
            reasoning: Some("the actual answer".to_string()),
            tool_calls: vec![],
        };
        let line = nojson::Json(&record).to_string();
        let parsed = parse_conversation_line(&line)
            .expect("parse must succeed")
            .expect("assistant record must yield a ChatMessage");
        match parsed {
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(content, "the actual answer");
                assert_eq!(reasoning_content.as_deref(), None);
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn tool_record_roundtrips_through_parse() {
        let record = SessionRecord::Tool {
            ts: 7,
            call_id: "call_x".to_string(),
            content: r#"{"ok":true}"#.to_string(),
        };
        let line = nojson::Json(&record).to_string();
        let parsed = parse_conversation_line(&line)
            .expect("parse must succeed")
            .expect("tool record must yield a ChatMessage");
        match parsed {
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "call_x");
                assert_eq!(content, r#"{"ok":true}"#);
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn summary_record_roundtrips_through_parse_summary_line() {
        let record = SessionRecord::Summary {
            ts: 100,
            since_ts: 10,
            cutoff_ts: 90,
            text: "user asked for X".to_string(),
        };
        let line = nojson::Json(&record).to_string();
        let parsed = parse_summary_line(&line)
            .expect("parse ok")
            .expect("summary yields SummaryRecord");
        assert_eq!(parsed.ts, 100);
        assert_eq!(parsed.since_ts, 10);
        assert_eq!(parsed.cutoff_ts, 90);
        assert_eq!(parsed.text, "user asked for X");
    }

    #[test]
    fn summary_record_is_not_returned_by_parse_conversation_line() {
        // Compaction API split: `load_conversation` (which uses
        // `parse_conversation_line`) must not surface summary
        // records as ChatMessages — those are exposed via
        // `load_summaries` instead.
        let record = SessionRecord::Summary {
            ts: 100,
            since_ts: 10,
            cutoff_ts: 90,
            text: "should not appear as ChatMessage".to_string(),
        };
        let line = nojson::Json(&record).to_string();
        let parsed = parse_conversation_line(&line).expect("parse ok");
        assert!(parsed.is_none());
    }

    #[test]
    fn token_usage_record_serialises_only_present_fields() {
        let record = SessionRecord::TokenUsage {
            ts: 42,
            body: TokenUsageBody {
                prompt_tokens: Some(1000),
                completion_tokens: None,
                total_tokens: Some(1050),
                prompt_cache_hit_tokens: Some(800),
                prompt_cache_miss_tokens: Some(200),
            },
        };
        let line = nojson::Json(&record).to_string();
        assert_eq!(
            line,
            r#"{"kind":"token_usage","ts":42,"usage":{"prompt_tokens":1000,"total_tokens":1050,"prompt_cache_hit_tokens":800,"prompt_cache_miss_tokens":200}}"#
        );
    }

    #[test]
    fn parse_prompt_tokens_returns_none_for_non_token_usage_lines() {
        let record = SessionRecord::User {
            ts: 1,
            text: "hi".to_string(),
        };
        let line = nojson::Json(&record).to_string();
        assert!(parse_prompt_tokens(&line).expect("parse ok").is_none());
    }

    #[test]
    fn parse_prompt_tokens_extracts_value_from_token_usage_line() {
        let record = SessionRecord::TokenUsage {
            ts: 42,
            body: TokenUsageBody {
                prompt_tokens: Some(17_000),
                ..Default::default()
            },
        };
        let line = nojson::Json(&record).to_string();
        assert_eq!(parse_prompt_tokens(&line).expect("parse ok"), Some(17_000));
    }

    #[test]
    fn parse_prompt_tokens_tolerates_missing_prompt_tokens_field() {
        let record = SessionRecord::TokenUsage {
            ts: 42,
            body: TokenUsageBody {
                prompt_tokens: None,
                total_tokens: Some(5),
                ..Default::default()
            },
        };
        let line = nojson::Json(&record).to_string();
        assert!(parse_prompt_tokens(&line).expect("parse ok").is_none());
    }

    #[test]
    fn parse_conversation_line_with_meta_carries_ts_for_user_records() {
        let record = SessionRecord::User {
            ts: 999,
            text: "hi".to_string(),
        };
        let line = nojson::Json(&record).to_string();
        match parse_conversation_line_with_meta(&line).expect("parse ok") {
            LineKind::Message { ts, message } => {
                assert_eq!(ts, 999);
                assert!(matches!(message, ChatMessage::User(_)));
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn parse_conversation_line_with_meta_recognises_summary_cutoff() {
        let record = SessionRecord::Summary {
            ts: 500,
            since_ts: 100,
            cutoff_ts: 450,
            text: "…".to_string(),
        };
        let line = nojson::Json(&record).to_string();
        match parse_conversation_line_with_meta(&line).expect("parse ok") {
            LineKind::Summary { cutoff_ts } => assert_eq!(cutoff_ts, 450),
            other => panic!("expected summary, got {other:?}"),
        }
    }

    #[test]
    fn plan_mode_roundtrips_through_file() {
        let dir = std::env::temp_dir().join(format!("attini-plan-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Missing file defaults to off.
        assert!(!load_plan_mode(&dir).unwrap());

        save_plan_mode(&dir, true).unwrap();
        assert!(load_plan_mode(&dir).unwrap());

        save_plan_mode(&dir, false).unwrap();
        assert!(!load_plan_mode(&dir).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_effort_roundtrips_through_file() {
        let dir = std::env::temp_dir().join(format!("attini-te-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Missing file defaults to high (thinking on).
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::High);

        save_thinking_effort(&dir, ThinkingEffort::Low).unwrap();
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::Low);

        save_thinking_effort(&dir, ThinkingEffort::High).unwrap();
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::High);

        save_thinking_effort(&dir, ThinkingEffort::Max).unwrap();
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::Max);

        save_thinking_effort(&dir, ThinkingEffort::None).unwrap();
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_effort_tolerates_bad_file_content() {
        let dir = std::env::temp_dir().join(format!("attini-te-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(THINKING_EFFORT_FILE), "not-an-effort").unwrap();
        assert_eq!(load_thinking_effort(&dir).unwrap(), ThinkingEffort::High);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_tolerates_bad_file_content() {
        let dir = std::env::temp_dir().join(format!("attini-plan-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(PLAN_MODE_FILE), "not-1").unwrap();
        assert!(!load_plan_mode(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    impl std::fmt::Debug for LineKind {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                LineKind::Message { ts, .. } => write!(f, "Message(ts={ts})"),
                LineKind::Summary { cutoff_ts } => write!(f, "Summary(cutoff_ts={cutoff_ts})"),
                LineKind::Other => write!(f, "Other"),
            }
        }
    }
}
