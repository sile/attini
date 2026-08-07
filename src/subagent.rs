//! Spawn and observe child `attini agent` invocations in separate
//! tmux windows. The parent invokes `subagent_start` to fork a child
//! and `subagent_wait` to block until the listed children reach a
//! non-running state. Availability is gated on `$TMUX` being set and
//! `$ATTINI_IS_SUBAGENT` being unset so children cannot recurse.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent_cli::shell_single_quote;
use crate::session::{
    InvocationEndReason, LockStatus, SessionRecord, inspect_lock, read_conversation_records,
    session_paths, session_root,
};

/// Ceiling on how long `spawn` waits for the child to acquire its
/// session LOCK before declaring the startup a failure.
const SPAWN_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval used both by the spawn barrier and by `wait`
/// when watching each child's LOCK state.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Number of times `spawn` retries the auto-generated session name
/// when the low-entropy hex suffix collides with an existing
/// `.attini/{name}/` directory.
const AUTOGEN_RETRY_ATTEMPTS: usize = 3;

/// Result of a successful `spawn` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedSubagent {
    /// Session name the child was started with (auto-generated when
    /// the caller did not supply one).
    pub session_name: String,
    /// tmux window id (`@<n>` form) captured from
    /// `tmux new-window -P -F '#{window_id}'`.
    pub window_id: String,
}

/// Snapshot of one child's terminal state, returned by `wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStatus {
    pub session_name: String,
    pub state: SubagentState,
    /// Latest Assistant `content` (or a `reasoning_content`
    /// fallback) from the child's `conversation.jsonl`. Empty when
    /// no usable Assistant record has been recorded yet.
    pub content: String,
}

/// State classification `wait` reports for each requested session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    /// The named session directory does not exist. `wait` skips
    /// polling for it and returns this state immediately so the
    /// caller can distinguish "unknown name" from "child crashed".
    NotFound,
    /// The child finished normally
    /// ([`InvocationEndReason::Completed`]).
    Completed,
    /// The child stopped in an approval-pending state; the tmux
    /// window is idle and `pending.json` is present until a human
    /// resumes it.
    AwaitingApproval,
    /// The child ended in an error state
    /// ([`InvocationEndReason::Error`] or
    /// [`InvocationEndReason::SessionToolCallExhausted`], collapsed).
    Error,
    /// The child released its LOCK without leaving a coherent
    /// terminal record (SIGKILL, OOM, panic-abort before
    /// `InvocationEnd` was appended, invariant-broken `pending.json`
    /// state, etc.). Callers should treat it as a failure and
    /// decide whether to retry.
    Crashed,
}

/// Failure modes for [`spawn`].
#[derive(Debug)]
pub enum SubagentError {
    /// A session directory with that name already exists. Auto-
    /// generated names retry a bounded number of times before
    /// surfacing this.
    AlreadyExists { session_name: String },
    /// Requested session name failed the shared validation applied
    /// to every attini session (empty, path separators, control
    /// characters, ...).
    InvalidSessionName { message: String },
    /// The `tmux new-window` invocation failed. `message` captures
    /// tmux's stderr.
    TmuxFailed { message: String },
    /// The child did not acquire its session LOCK within the
    /// spawn barrier timeout (`SPAWN_STARTUP_TIMEOUT` in this
    /// module).
    StartupTimeout { session_name: String },
    /// Filesystem or I/O error not covered by the more specific
    /// variants (e.g. the parent could not read its own CWD).
    Io(io::Error),
}

impl SubagentError {
    /// Split into `(error_code, human_message)` for
    /// `tool_error_json` in `agent_cli`.
    pub fn to_code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::AlreadyExists { session_name } => (
                "subagent_already_exists",
                format!("session {session_name:?} already exists"),
            ),
            Self::InvalidSessionName { message } => (
                "subagent_invalid_session_name",
                format!("invalid session name: {message}"),
            ),
            Self::TmuxFailed { message } => (
                "subagent_start_failed",
                format!("tmux new-window failed: {message}"),
            ),
            Self::StartupTimeout { session_name } => (
                "subagent_startup_timeout",
                format!(
                    "child for session {session_name:?} did not acquire its LOCK within {}s",
                    SPAWN_STARTUP_TIMEOUT.as_secs()
                ),
            ),
            Self::Io(e) => ("subagent_start_failed", format!("io error: {e}")),
        }
    }
}

impl std::fmt::Display for SubagentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, msg) = self.to_code_and_message();
        f.write_str(&msg)
    }
}

impl From<io::Error> for SubagentError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Spawn `attini agent -s <session_name> <prompt>` in a new tmux
/// window, returning once the child has acquired its session LOCK.
pub fn spawn(session_name: Option<&str>, prompt: &str) -> Result<SpawnedSubagent, SubagentError> {
    let session_name = resolve_session_name(session_name)?;
    pre_create_session_dir(&session_name)?;
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd
        .to_str()
        .ok_or_else(|| SubagentError::Io(io::Error::other("CWD is not valid UTF-8")))?;
    let shell_command = quote_shell_command(&["attini", "agent", "-s", &session_name, prompt]);
    let output = Command::new("tmux")
        .args([
            "new-window",
            "-d",
            "-c",
            cwd_str,
            "-P",
            "-F",
            "#{window_id}",
            "-n",
            &session_name,
            "-e",
            "ATTINI_IS_SUBAGENT=1",
            &shell_command,
        ])
        .output()
        .map_err(|e| SubagentError::TmuxFailed {
            message: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(SubagentError::TmuxFailed {
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let window_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    wait_for_lock_acquisition(&session_name)?;
    Ok(SpawnedSubagent {
        session_name,
        window_id,
    })
}

/// Block until every listed session reaches a non-running state,
/// then return one [`SubagentStatus`] per session in the input
/// order. Sessions whose `.attini/{name}/` directory does not exist
/// short-circuit to [`SubagentState::NotFound`] without polling.
pub fn wait(session_names: &[String]) -> Vec<SubagentStatus> {
    // Separate names into (not_found, needs_polling) up front so the
    // main loop only touches the polling set.
    let mut not_found: Vec<String> = Vec::new();
    let mut polling: Vec<String> = Vec::new();
    for name in session_names {
        if session_dir(name).is_dir() {
            polling.push(name.clone());
        } else {
            not_found.push(name.clone());
        }
    }
    let mut done: std::collections::HashMap<String, SubagentStatus> =
        std::collections::HashMap::new();
    for name in &not_found {
        done.insert(
            name.clone(),
            SubagentStatus {
                session_name: name.clone(),
                state: SubagentState::NotFound,
                content: String::new(),
            },
        );
    }
    while done.len() < session_names.len() {
        let mut made_progress = false;
        for name in &polling {
            if done.contains_key(name) {
                continue;
            }
            let dir = session_dir(name);
            let lock = inspect_lock(&dir.join("LOCK"));
            if matches!(lock, LockStatus::PidAlive(_)) {
                continue;
            }
            let pending_exists = dir.join("pending.json").try_exists().unwrap_or(false);
            let records =
                read_conversation_records(&dir.join("conversation.jsonl")).unwrap_or_default();
            let latest_end_reason = latest_invocation_end_reason(&records);
            let state = classify_state(pending_exists, latest_end_reason);
            let content = pick_last_content(&records);
            done.insert(
                name.clone(),
                SubagentStatus {
                    session_name: name.clone(),
                    state,
                    content,
                },
            );
            made_progress = true;
        }
        if !made_progress && done.len() < session_names.len() {
            thread::sleep(POLL_INTERVAL);
        }
    }
    session_names
        .iter()
        .map(|n| done.remove(n).expect("every requested name was resolved"))
        .collect()
}

fn resolve_session_name(supplied: Option<&str>) -> Result<String, SubagentError> {
    if let Some(name) = supplied {
        // Reuse session_paths' validation for path-safe / control-
        // character checks so subagent-created sessions cannot land
        // on paths the rest of attini would refuse.
        session_paths(name).map_err(|e| SubagentError::InvalidSessionName {
            message: e.to_string(),
        })?;
        return Ok(name.to_string());
    }
    for _ in 0..AUTOGEN_RETRY_ATTEMPTS {
        let candidate = generate_auto_name();
        if !session_dir(&candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(SubagentError::AlreadyExists {
        session_name: "<auto-generated>".to_string(),
    })
}

/// `subagent-{YYYYMMDD-HHMMSS}-{6 hex}`. Uses `subsec_nanos()` &
/// `0xFF_FF_FF` as the hex source; the `spawn` retry loop rerolls
/// on collision so the granularity is coarse enough to only rely on
/// a handful of retries in the worst case.
fn generate_auto_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    let hex = now.subsec_nanos() & 0x00FF_FFFF;
    format!("subagent-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{hex:06x}")
}

/// Very small pure UTC breakdown of a Unix timestamp so we don't
/// pull a `chrono`-shaped dependency for the auto-name suffix.
/// Only correct on the proleptic Gregorian calendar for
/// 1970-01-01 <= t < 2100-01-01, which is plenty for a
/// millisecond-granularity id.
fn unix_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let day = secs / 86_400;
    let time_of_day = (secs % 86_400) as u32;
    let h = time_of_day / 3600;
    let mi = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    // days since 1970-01-01 -> Y/M/D via Howard Hinnant's civil-
    // from-days algorithm (public domain).
    let day = day as i64 + 719_468;
    let era = if day >= 0 { day } else { day - 146_096 } / 146_097;
    let doe = (day - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as u32, m, d, h, mi, s)
}

fn pre_create_session_dir(name: &str) -> Result<(), SubagentError> {
    fs::create_dir_all(session_root())?;
    match fs::create_dir(session_dir(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(SubagentError::AlreadyExists {
            session_name: name.to_string(),
        }),
        Err(e) => Err(SubagentError::Io(e)),
    }
}

fn wait_for_lock_acquisition(name: &str) -> Result<(), SubagentError> {
    let deadline = Instant::now() + SPAWN_STARTUP_TIMEOUT;
    let lock_path = session_dir(name).join("LOCK");
    loop {
        if matches!(inspect_lock(&lock_path), LockStatus::PidAlive(_)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(SubagentError::StartupTimeout {
                session_name: name.to_string(),
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn session_dir(name: &str) -> PathBuf {
    session_root().join(name)
}

fn latest_invocation_end_reason(records: &[SessionRecord]) -> Option<InvocationEndReason> {
    for r in records.iter().rev() {
        if let SessionRecord::InvocationEnd { reason, .. } = r {
            return Some(*reason);
        }
    }
    None
}

/// Decision table from the polished issue: pending.json existence
/// combined with the latest InvocationEnd.reason picks one of the
/// four terminal states (`NotFound` is set upstream in `wait`).
pub(crate) fn classify_state(
    pending_exists: bool,
    latest_end_reason: Option<InvocationEndReason>,
) -> SubagentState {
    if pending_exists {
        return SubagentState::AwaitingApproval;
    }
    match latest_end_reason {
        Some(InvocationEndReason::Completed) => SubagentState::Completed,
        Some(InvocationEndReason::Error) => SubagentState::Error,
        Some(InvocationEndReason::SessionToolCallExhausted) => SubagentState::Error,
        // `AwaitingApproval` reason without pending.json means the
        // invariant broke somewhere. Treat it as a crash so the
        // parent can retry or surface it to the user.
        Some(InvocationEndReason::AwaitingApproval) => SubagentState::Crashed,
        None => SubagentState::Crashed,
    }
}

/// Same three-step fallback as `pick_summary_text`: `content` →
/// `reasoning` → `""`, applied to the newest Assistant record in
/// the conversation.
pub(crate) fn pick_last_content(records: &[SessionRecord]) -> String {
    for r in records.iter().rev() {
        if let SessionRecord::Assistant {
            content, reasoning, ..
        } = r
        {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
            if let Some(r) = reasoning {
                let trimmed = r.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            return String::new();
        }
    }
    String::new()
}

/// Compose a shell command by POSIX-single-quoting each argv element
/// and joining with spaces. Every element goes through
/// [`shell_single_quote`], which always wraps in single quotes and
/// escapes interior quotes with the `'\''` sequence, so the output
/// is safe to hand to a shell's `-c` no matter what characters the
/// arguments contain.
pub(crate) fn quote_shell_command(argv: &[&str]) -> String {
    argv.iter()
        .map(|a| shell_single_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // classify_state
    // -----------------------------------------------------------------

    #[test]
    fn classify_pending_wins_over_reason() {
        assert_eq!(
            classify_state(true, Some(InvocationEndReason::Completed)),
            SubagentState::AwaitingApproval
        );
        assert_eq!(
            classify_state(true, Some(InvocationEndReason::Error)),
            SubagentState::AwaitingApproval
        );
        assert_eq!(classify_state(true, None), SubagentState::AwaitingApproval);
    }

    #[test]
    fn classify_completed_maps_to_completed() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::Completed)),
            SubagentState::Completed
        );
    }

    #[test]
    fn classify_error_and_exhausted_collapse_to_error() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::Error)),
            SubagentState::Error
        );
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::SessionToolCallExhausted)),
            SubagentState::Error
        );
    }

    #[test]
    fn classify_awaiting_without_pending_is_crashed() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::AwaitingApproval)),
            SubagentState::Crashed
        );
    }

    #[test]
    fn classify_no_invocation_end_is_crashed() {
        assert_eq!(classify_state(false, None), SubagentState::Crashed);
    }

    // -----------------------------------------------------------------
    // pick_last_content
    // -----------------------------------------------------------------

    fn assistant(content: &str, reasoning: Option<&str>) -> SessionRecord {
        SessionRecord::Assistant {
            ts: 0,
            content: content.to_string(),
            reasoning: reasoning.map(str::to_string),
            tool_calls: Vec::new(),
        }
    }

    #[test]
    fn pick_last_content_returns_content_when_present() {
        let records = vec![assistant("first", None), assistant("last message", None)];
        assert_eq!(pick_last_content(&records), "last message");
    }

    #[test]
    fn pick_last_content_falls_back_to_reasoning() {
        let records = vec![assistant("  ", Some("actual answer"))];
        assert_eq!(pick_last_content(&records), "actual answer");
    }

    #[test]
    fn pick_last_content_returns_empty_when_reasoning_absent() {
        let records = vec![assistant("", None)];
        assert_eq!(pick_last_content(&records), "");
    }

    #[test]
    fn pick_last_content_returns_empty_when_no_assistant_record() {
        let records = vec![SessionRecord::User {
            ts: 0,
            text: "hi".to_string(),
        }];
        assert_eq!(pick_last_content(&records), "");
    }

    #[test]
    fn pick_last_content_uses_newest_assistant_only() {
        // Even if the latest Assistant record is empty and older
        // ones have content, we honor the "latest" rule.
        let records = vec![assistant("older", None), assistant("", None)];
        assert_eq!(pick_last_content(&records), "");
    }

    // -----------------------------------------------------------------
    // quote_shell_command
    // -----------------------------------------------------------------

    #[test]
    fn quote_shell_command_wraps_each_argv_element() {
        assert_eq!(
            quote_shell_command(&["attini", "agent", "-s", "test", "hello"]),
            "'attini' 'agent' '-s' 'test' 'hello'"
        );
    }

    #[test]
    fn quote_shell_command_handles_metacharacters() {
        // Verify that a full alphabet of shell hazards survives
        // round-tripping through a shell's word splitter.
        let raw = vec!["a b", "$FOO", "`cmd`", "\\", "\"quoted\"", "'single'"];
        let escaped = quote_shell_command(&raw);
        // The naive property: the escape function's output must
        // interpolate literally under sh word splitting. We assert
        // that each escaped token, on its own, contains the raw
        // characters verbatim between the outermost quotes.
        for raw_token in &raw {
            // Interior single quotes are the tricky case; other
            // metacharacters must appear literally inside the outer
            // '...' block.
            if !raw_token.contains('\'') {
                assert!(
                    escaped.contains(&format!("'{raw_token}'")),
                    "token {raw_token:?} not in {escaped}"
                );
            }
        }
        // Interior single quote uses the '\'' bounce escape.
        assert!(
            escaped.contains(r#"'single'\''single'\'''"#)
                || escaped.contains(r#"''\''single'\'''"#),
            "single-quote escape missing in {escaped}"
        );
    }

    #[test]
    fn quote_shell_command_wraps_empty_string() {
        assert_eq!(quote_shell_command(&[""]), "''");
    }

    // -----------------------------------------------------------------
    // generate_auto_name
    // -----------------------------------------------------------------

    #[test]
    fn generate_auto_name_matches_expected_shape() {
        let name = generate_auto_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 4, "want 4 dash-separated parts, got {name}");
        assert_eq!(parts[0], "subagent");
        assert_eq!(parts[1].len(), 8, "date part should be 8 chars: {name}");
        assert_eq!(parts[2].len(), 6, "time part should be 6 chars: {name}");
        assert_eq!(parts[3].len(), 6, "hex part should be 6 chars: {name}");
        assert!(
            parts[3].chars().all(|c| c.is_ascii_hexdigit()),
            "hex part not hex: {name}"
        );
    }
}
