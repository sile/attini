//! Run a child `attini agent` synchronously in a separate session.
//! The parent invokes the single `subagent_run` tool to self-exec the
//! current attini binary as a child process and blocks until the
//! child reaches a terminal state. Availability is gated on
//! `$ATTINI_IS_SUBAGENT` being unset so children cannot recurse.

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sansio::permissions::Mode;
use crate::session::{
    InvocationEndReason, LockStatus, SessionRecord, inspect_lock, read_conversation_records,
    session_paths, session_root,
};

/// Number of times `run` retries the auto-generated session name
/// when the low-entropy hex suffix collides with an existing
/// `.attini/{name}/` directory.
const AUTOGEN_RETRY_ATTEMPTS: usize = 3;

/// Result of a successful `run` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStatus {
    /// Session name the child ran in (auto-generated when the caller
    /// did not supply one).
    pub session_name: String,
    pub state: SubagentState,
    /// Latest Assistant `content` (or a `reasoning_content`
    /// fallback) from the child's `conversation.jsonl`. Empty when
    /// no usable Assistant record has been recorded yet.
    pub content: String,
}

/// Terminal-state classification `run` reports for the child session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    /// The child finished normally
    /// ([`InvocationEndReason::Completed`]).
    Completed,
    /// The child stopped in an approval-pending state; `pending.json`
    /// is present until a human resumes the session with
    /// `attini agent -s <session> --approve` / `--reject`.
    AwaitingApproval,
    /// The child ended in an error state
    /// ([`InvocationEndReason::Error`] or
    /// [`InvocationEndReason::SessionToolCallExhausted`], collapsed).
    Error,
    /// The child exited without leaving a coherent terminal record
    /// (SIGKILL, OOM, panic-abort before `InvocationEnd` was
    /// appended, invariant-broken `pending.json` state, etc.). Callers
    /// should treat it as a failure and decide whether to retry.
    Crashed,
}

/// Failure modes for [`run`].
#[derive(Debug)]
pub enum SubagentError {
    /// Requested session name failed the shared validation applied
    /// to every attini session (empty, path separators, control
    /// characters, ...).
    InvalidSessionName { message: String },
    /// Auto-generated names collided with existing session
    /// directories across `AUTOGEN_RETRY_ATTEMPTS` attempts.
    NameGenerationExhausted,
    /// The named session is in use: its LOCK names a live PID, so a
    /// concurrent invocation holds it.
    Busy { session_name: String },
    /// `std::env::current_exe()` or `Command::spawn` failed.
    SpawnFailed { message: String },
    /// Filesystem or I/O error not covered by the more specific
    /// variants.
    Io(io::Error),
}

impl SubagentError {
    /// Split into `(error_code, human_message)` for
    /// `tool_error_json` in `agent_cli`.
    pub fn to_code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::InvalidSessionName { message } => (
                "subagent_invalid_session_name",
                format!("invalid session name: {message}"),
            ),
            Self::NameGenerationExhausted => (
                "subagent_name_generation_exhausted",
                format!(
                    "could not generate a unique session name after {AUTOGEN_RETRY_ATTEMPTS} \
                     attempts"
                ),
            ),
            Self::Busy { session_name } => (
                "subagent_session_busy",
                format!("session {session_name:?} is busy (LOCK held by a live process)"),
            ),
            Self::SpawnFailed { message } => (
                "subagent_run_failed",
                format!("failed to start child agent: {message}"),
            ),
            Self::Io(e) => ("subagent_run_failed", format!("io error: {e}")),
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

/// Run `attini agent --model <model> -s <session_name> <prompt>` as a
/// synchronous child process, inheriting the parent's working
/// directory and environment plus `ATTINI_IS_SUBAGENT=1`. Returns
/// once the child reaches a terminal state.
pub fn run(
    session_name: Option<&str>,
    prompt: &str,
    model: &str,
    mode: Mode,
) -> Result<SubagentStatus, SubagentError> {
    let session_name = resolve_session_name(session_name)?;
    let paths = session_paths(&session_name)?;
    let pending_exists_preflight = paths.pending.try_exists().unwrap_or(false);
    match preflight(
        paths.dir.is_dir(),
        inspect_lock(&paths.lock),
        pending_exists_preflight,
    ) {
        Preflight::Spawn => {}
        Preflight::AwaitingApproval => {
            let records = read_conversation_records(&paths.conversation).unwrap_or_default();
            return Ok(SubagentStatus {
                session_name,
                state: SubagentState::AwaitingApproval,
                content: pick_last_content(&records),
            });
        }
        Preflight::Busy => {
            return Err(SubagentError::Busy { session_name });
        }
    }

    let exe = std::env::current_exe().map_err(|e| SubagentError::SpawnFailed {
        message: format!("current_exe: {e}"),
    })?;
    let mut cmd = Command::new(exe);
    cmd.arg("agent")
        .arg("--model")
        .arg(model)
        .arg("-s")
        .arg(&session_name)
        .env("ATTINI_IS_SUBAGENT", "1");
    match mode {
        Mode::LocalOnly => {
            cmd.arg("--local-only");
        }
        Mode::Default => {}
    }
    cmd.arg(prompt);
    // The child's stdout (its streaming model progress) is piped and
    // displayed to the parent stderr under the shared rate limit;
    // its stderr is inherited raw. The tool result content still comes
    // from the child session's conversation, not from this stream.
    let status = crate::child_output::run_streaming_stdout(&mut cmd).map_err(|e| {
        SubagentError::SpawnFailed {
            message: e.to_string(),
        }
    })?;
    let exit_success = status.success();

    let pending_exists = paths.pending.try_exists().unwrap_or(false);
    let records = read_conversation_records(&paths.conversation).unwrap_or_default();
    let latest_end_reason = latest_invocation_end_reason(&records);
    let state = classify_state(pending_exists, latest_end_reason, exit_success);
    let content = pick_last_content(&records);
    Ok(SubagentStatus {
        session_name,
        state,
        content,
    })
}

/// Decision made before spawning a child for the named session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Preflight {
    /// Fresh session (no directory yet) or idle existing session:
    /// spawn the child.
    Spawn,
    /// The session is stopped in an approval-pending state; do not
    /// spawn, report `AwaitingApproval` to the caller.
    AwaitingApproval,
    /// A live LOCK is held: another invocation owns the session.
    Busy,
}

/// Classify a session directory's state before deciding whether to
/// spawn. An existing session is reusable only when idle and not
/// holding a live LOCK; a session stopped at an approval-pending
/// state must not receive a new prompt.
pub(crate) fn preflight(dir_exists: bool, lock: LockStatus, pending_exists: bool) -> Preflight {
    if !dir_exists {
        return Preflight::Spawn;
    }
    if matches!(lock, LockStatus::PidAlive(_)) {
        return Preflight::Busy;
    }
    if pending_exists {
        return Preflight::AwaitingApproval;
    }
    Preflight::Spawn
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
    Err(SubagentError::NameGenerationExhausted)
}

/// `subagent-{YYYYMMDD-HHMMSS}-{6 hex}`. Uses `subsec_nanos()` &
/// `0xFF_FF_FF` as the hex source; the retry loop rerolls on
/// collision so the granularity is coarse enough to only rely on
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

/// Decision table for the terminal state: `pending.json` existence
/// combined with the latest `InvocationEnd` reason and the child's
/// exit status. A `Completed` reason with a non-zero exit is treated
/// as a crash (the record and the process disagreed).
pub(crate) fn classify_state(
    pending_exists: bool,
    latest_end_reason: Option<InvocationEndReason>,
    exit_success: bool,
) -> SubagentState {
    if pending_exists {
        return SubagentState::AwaitingApproval;
    }
    match latest_end_reason {
        Some(InvocationEndReason::Completed) if exit_success => SubagentState::Completed,
        Some(InvocationEndReason::Completed) => SubagentState::Crashed,
        Some(InvocationEndReason::Error) | Some(InvocationEndReason::SessionToolCallExhausted) => {
            SubagentState::Error
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // preflight
    // -----------------------------------------------------------------

    #[test]
    fn preflight_fresh_session_spawns() {
        assert_eq!(preflight(false, LockStatus::None, false), Preflight::Spawn);
    }

    #[test]
    fn preflight_idle_existing_session_spawns() {
        assert_eq!(preflight(true, LockStatus::None, false), Preflight::Spawn);
        assert_eq!(
            preflight(true, LockStatus::PidDead, false),
            Preflight::Spawn
        );
        assert_eq!(
            preflight(true, LockStatus::Corrupted, false),
            Preflight::Spawn
        );
    }

    #[test]
    fn preflight_live_lock_is_busy() {
        assert_eq!(
            preflight(true, LockStatus::PidAlive(42), false),
            Preflight::Busy
        );
        assert_eq!(
            preflight(true, LockStatus::PidAlive(42), true),
            Preflight::Busy
        );
    }

    #[test]
    fn preflight_pending_without_live_lock_awaits_approval() {
        assert_eq!(
            preflight(true, LockStatus::None, true),
            Preflight::AwaitingApproval
        );
    }

    // -----------------------------------------------------------------
    // classify_state
    // -----------------------------------------------------------------

    #[test]
    fn classify_pending_wins_over_reason_and_exit() {
        assert_eq!(
            classify_state(true, Some(InvocationEndReason::Completed), true),
            SubagentState::AwaitingApproval
        );
        assert_eq!(
            classify_state(true, Some(InvocationEndReason::Error), false),
            SubagentState::AwaitingApproval
        );
        assert_eq!(
            classify_state(true, None, false),
            SubagentState::AwaitingApproval
        );
    }

    #[test]
    fn classify_completed_with_clean_exit_is_completed() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::Completed), true),
            SubagentState::Completed
        );
    }

    #[test]
    fn classify_completed_with_bad_exit_is_crashed() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::Completed), false),
            SubagentState::Crashed
        );
    }

    #[test]
    fn classify_error_and_exhausted_collapse_to_error() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::Error), false),
            SubagentState::Error
        );
        assert_eq!(
            classify_state(
                false,
                Some(InvocationEndReason::SessionToolCallExhausted),
                false
            ),
            SubagentState::Error
        );
    }

    #[test]
    fn classify_awaiting_without_pending_is_crashed() {
        assert_eq!(
            classify_state(false, Some(InvocationEndReason::AwaitingApproval), true),
            SubagentState::Crashed
        );
    }

    #[test]
    fn classify_no_invocation_end_is_crashed() {
        assert_eq!(classify_state(false, None, true), SubagentState::Crashed);
        assert_eq!(classify_state(false, None, false), SubagentState::Crashed);
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
