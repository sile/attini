//! Filesystem-backed session storage for the CLI agent
//! (`attini agent`).
//!
//! Each session lives under `.attini/{SESSION_NAME}/` relative to
//! the current working directory:
//!
//! - `LOCK` — exclusive lock file created with `O_EXCL` on open.
//!   Removed on graceful `Session::close`. A stale file (from a
//!   crashed run) must be removed manually.
//! - `conversation.jsonl` — append-only history. One JSON object
//!   per line, tagged by `kind`. User / assistant / tool records
//!   are the source of truth for reconstructing conversation
//!   context on subsequent invocations.
//! - `pending.json` — present iff the previous invocation
//!   suspended waiting for approval. Absent otherwise.
//!
//! Spike scope; not production-hardened.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nojson::{DisplayJson, Json, JsonFormatter, RawJson};

use crate::sansio::deepseek::{ChatMessage, ToolCall};

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
}

impl Session {
    /// Open (or create) the session directory for `name` under
    /// `.attini/` in the current working directory. Acquires the
    /// exclusive lock; returns `Err` if another process already
    /// holds it (or crashed with a stale LOCK).
    pub fn open(name: &str) -> io::Result<Self> {
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
        let dir = PathBuf::from(".attini").join(name);
        fs::create_dir_all(&dir)?;
        let lock_path = dir.join("LOCK");
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "session {name:?} is locked ({}). Remove it manually if the previous invocation crashed.",
                            lock_path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
        let conversation_path = dir.join("conversation.jsonl");
        let pending_path = dir.join("pending.json");
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&conversation_path)?;
        Ok(Self {
            dir,
            lock_path,
            _lock: lock,
            conversation_path,
            pending_path,
            writer,
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

    /// Append a record to `conversation.jsonl` and flush.
    pub fn append(&mut self, record: &SessionRecord) -> io::Result<()> {
        let mut line = Json(record).to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }

    /// Write `pending.json` (overwriting any previous). The
    /// caller should follow up by exiting the process — the
    /// pending file signals to the next invocation that the
    /// agent loop is mid-turn.
    pub fn save_pending(&self, pending: &Pending) -> io::Result<()> {
        let content = Json(pending).to_string();
        fs::write(&self.pending_path, content)
    }

    /// Read `pending.json` if it exists.
    pub fn load_pending(&self) -> io::Result<Option<Pending>> {
        let text = match fs::read_to_string(&self.pending_path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let json =
            RawJson::parse(&text).map_err(|e| io::Error::other(format!("pending.json: {e}")))?;
        Pending::from_json(json.value()).map(Some)
    }

    /// Remove `pending.json` after a resume has been applied.
    pub fn clear_pending(&self) -> io::Result<()> {
        match fs::remove_file(&self.pending_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort unlock. Explicit `close()` returns errors;
        // Drop swallows them.
        let _ = fs::remove_file(&self.lock_path);
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
    },
    /// Snapshot of transport / agent metric counters. Emitted
    /// once at invocation end.
    MetricsSnapshot {
        ts: u64,
        counters: MetricsSnapshotBody,
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
}

impl InvocationEndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Error => "error",
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
            } => f.object(|f| {
                f.member("kind", "tool_approval")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("decision", decision.as_str())
            }),
            Self::MetricsSnapshot { ts, counters } => f.object(|f| {
                f.member("kind", "metrics_snapshot")?;
                f.member("ts", ts)?;
                f.member("counters", counters)
            }),
        }
    }
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
            Ok(Some(ChatMessage::Assistant {
                content,
                reasoning_content: reasoning,
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
        let function_name = read_string(item, "function_name")?;
        let arguments_json = read_string(item, "arguments_json")?;
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
    }

    #[test]
    fn pending_tool_kind_roundtrips() {
        for kind in [PendingToolKind::Patch, PendingToolKind::Command] {
            assert_eq!(PendingToolKind::parse(kind.as_str()), Some(kind));
        }
        assert!(PendingToolKind::parse("bogus").is_none());
    }
}
