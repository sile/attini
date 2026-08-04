//! JSON Lines transcript for `attini tui` sessions.
//!
//! The shell in [`crate::tui`] optionally hands every observable
//! conversation event, tool approval, and stream chunk to
//! [`TranscriptWriter`], which serialises records with [`nojson`] and
//! appends them to a caller-specified file on a dedicated tokio task.
//!
//! Design highlights:
//!
//! - Each line is a self-contained JSON object with a `kind` tag and
//!   a `ts` field in unix milliseconds. Readers that see an unknown
//!   `kind` are expected to skip the whole line and keep going.
//! - The writer task owns the file. Callers only touch a
//!   [`mpsc::UnboundedSender`] so shell-side sends never block the
//!   `tokio::select!` loop.
//! - Open failure is surfaced synchronously via
//!   [`TranscriptWriter::open`] returning `Err`. Write / flush
//!   failure at runtime is reported once through an [`oneshot`]
//!   channel; further records are silently dropped
//!   (silent-after-first) so a broken sink can never disturb the
//!   TUI display.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use nojson::{DisplayJson, Json, JsonFormatter};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Take a unix-milliseconds timestamp from the wall clock. Falls
/// back to `0` if the clock is set before the epoch (should be
/// unreachable on any live system).
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One entry appended to the JSON Lines transcript. See the module
/// doc for the on-disk format contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptRecord {
    SessionStart {
        ts: u64,
        attini_version: String,
        model: String,
        workspace: String,
    },
    UserMessage {
        ts: u64,
        text: String,
    },
    AssistantMessage {
        ts: u64,
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<AssistantToolCall>,
    },
    ToolResult {
        ts: u64,
        call_id: String,
        content: String,
    },
    PatchPreviewReady {
        ts: u64,
        call_id: String,
        target_paths: Vec<String>,
        added_lines: u64,
        removed_lines: u64,
        edit_count: u64,
    },
    ToolApproval {
        ts: u64,
        call_id: String,
        decision: ApprovalDecision,
    },
    CommandOutputChunk {
        ts: u64,
        call_id: String,
        stream: CommandStream,
        bytes_len: u64,
        preview: String,
    },
    Cancel {
        ts: u64,
    },
    TransportError {
        ts: u64,
        message: String,
    },
    Finish {
        ts: u64,
        reason: Option<String>,
    },
    SessionEnd {
        ts: u64,
        reason: SessionEndReason,
    },
}

/// Tool call attached to an assistant message. Mirrors
/// `sansio::deepseek::ToolCall` but is decoupled so the record
/// format does not follow API changes in wire types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments_json: String,
}

/// Approval outcome recorded before the corresponding event is fed
/// into the core.
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

/// Which pipe the chunk arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStream {
    Stdout,
    Stderr,
}

impl CommandStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Why the shell tore the session down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndReason {
    UserQuit,
    Eof,
}

impl SessionEndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::UserQuit => "user_quit",
            Self::Eof => "eof",
        }
    }
}

/// Maximum bytes of the sanitized chunk retained in the preview
/// field. The full stream is available in the eventual `tool_result`
/// record, so this exists only to make the transcript scannable.
pub const COMMAND_OUTPUT_PREVIEW_MAX_BYTES: usize = 200;

impl TranscriptRecord {
    /// Convenience: render this record as a single JSON string
    /// (without the trailing newline). Kept public for tests and
    /// external ad-hoc inspection.
    pub fn to_json_string(&self) -> String {
        Json(self).to_string()
    }

    /// Extract at most [`COMMAND_OUTPUT_PREVIEW_MAX_BYTES`] leading
    /// bytes of a sanitized command chunk and decode them lossily.
    /// The caller already stripped ANSI / C0 / BiDi noise upstream.
    pub fn command_preview_from_bytes(bytes: &[u8]) -> String {
        let end = bytes.len().min(COMMAND_OUTPUT_PREVIEW_MAX_BYTES);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}

impl DisplayJson for TranscriptRecord {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        match self {
            Self::SessionStart {
                ts,
                attini_version,
                model,
                workspace,
            } => f.object(|f| {
                f.member("kind", "session_start")?;
                f.member("ts", ts)?;
                f.member("attini_version", attini_version)?;
                f.member("model", model)?;
                f.member("workspace", workspace)
            }),
            Self::UserMessage { ts, text } => f.object(|f| {
                f.member("kind", "user_message")?;
                f.member("ts", ts)?;
                f.member("text", text)
            }),
            Self::AssistantMessage {
                ts,
                content,
                reasoning,
                tool_calls,
            } => f.object(|f| {
                f.member("kind", "assistant_message")?;
                f.member("ts", ts)?;
                f.member("content", content)?;
                f.member("reasoning", reasoning)?;
                f.member("tool_calls", tool_calls)
            }),
            Self::ToolResult {
                ts,
                call_id,
                content,
            } => f.object(|f| {
                f.member("kind", "tool_result")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("content", content)
            }),
            Self::PatchPreviewReady {
                ts,
                call_id,
                target_paths,
                added_lines,
                removed_lines,
                edit_count,
            } => f.object(|f| {
                f.member("kind", "patch_preview_ready")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("target_paths", target_paths)?;
                f.member("added_lines", added_lines)?;
                f.member("removed_lines", removed_lines)?;
                f.member("edit_count", edit_count)
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
            Self::CommandOutputChunk {
                ts,
                call_id,
                stream,
                bytes_len,
                preview,
            } => f.object(|f| {
                f.member("kind", "command_output_chunk")?;
                f.member("ts", ts)?;
                f.member("call_id", call_id)?;
                f.member("stream", stream.as_str())?;
                f.member("bytes_len", bytes_len)?;
                f.member("preview", preview)
            }),
            Self::Cancel { ts } => f.object(|f| {
                f.member("kind", "cancel")?;
                f.member("ts", ts)
            }),
            Self::TransportError { ts, message } => f.object(|f| {
                f.member("kind", "transport_error")?;
                f.member("ts", ts)?;
                f.member("message", message)
            }),
            Self::Finish { ts, reason } => f.object(|f| {
                f.member("kind", "finish")?;
                f.member("ts", ts)?;
                f.member("reason", reason)
            }),
            Self::SessionEnd { ts, reason } => f.object(|f| {
                f.member("kind", "session_end")?;
                f.member("ts", ts)?;
                f.member("reason", reason.as_str())
            }),
        }
    }
}

impl DisplayJson for AssistantToolCall {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("id", &self.id)?;
            f.member("function_name", &self.function_name)?;
            f.member("arguments_json", &self.arguments_json)
        })
    }
}

/// Handle to a background writer task. Owns the mpsc sender and the
/// join handle so callers only see the small [`send`] /
/// [`shutdown`] surface.
///
/// [`send`]: TranscriptWriter::send
/// [`shutdown`]: TranscriptWriter::shutdown
#[derive(Debug)]
pub struct TranscriptWriter {
    tx: mpsc::UnboundedSender<TranscriptRecord>,
    join: JoinHandle<()>,
}

impl TranscriptWriter {
    /// Open `path` for append (creating it if missing), start a
    /// background writer task, and emit a `session_start` record.
    ///
    /// The returned `oneshot::Receiver` fires at most once with a
    /// human-readable error message if the writer task hits a write
    /// or flush failure. The shell wires this into a fifth
    /// `tokio::select!` arm and displays the message in the sticky
    /// error banner.
    pub async fn open(
        path: impl AsRef<Path>,
        attini_version: String,
        model: String,
        workspace: String,
    ) -> std::io::Result<(Self, oneshot::Receiver<String>)> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<TranscriptRecord>();
        let (err_tx, err_rx) = oneshot::channel::<String>();

        // Seed the queue with the session_start record before
        // spawning so its ordering is guaranteed even under
        // aggressive scheduling.
        let seed = TranscriptRecord::SessionStart {
            ts: now_unix_millis(),
            attini_version,
            model,
            workspace,
        };
        // Send must succeed since we just created the channel and
        // the receiver is alive until we move it into the task.
        tx.send(seed)
            .expect("seed session_start onto fresh mpsc channel");

        let join = tokio::spawn(writer_loop(file, rx, err_tx));
        Ok((Self { tx, join }, err_rx))
    }

    /// Enqueue `record` for the writer task. Silently drops the
    /// record if the writer task has exited (which only happens
    /// after [`shutdown`] runs or after the runtime is torn down).
    ///
    /// [`shutdown`]: TranscriptWriter::shutdown
    pub fn send(&self, record: TranscriptRecord) {
        let _ = self.tx.send(record);
    }

    /// Enqueue a final `session_end` record, close the channel, and
    /// await the writer task's exit so the file is flushed before
    /// the shell tears the runtime down.
    pub async fn shutdown(self, reason: SessionEndReason) {
        let Self { tx, join } = self;
        let _ = tx.send(TranscriptRecord::SessionEnd {
            ts: now_unix_millis(),
            reason,
        });
        drop(tx);
        let _ = join.await;
    }
}

async fn writer_loop(
    mut file: tokio::fs::File,
    mut rx: mpsc::UnboundedReceiver<TranscriptRecord>,
    err_tx: oneshot::Sender<String>,
) {
    // Take-once slot: the first write/flush failure sends through
    // err_tx and every subsequent record is dropped silently.
    let mut err_tx = Some(err_tx);
    while let Some(record) = rx.recv().await {
        if err_tx.is_none() {
            continue;
        }
        let mut line = record.to_json_string();
        line.push('\n');
        if let Err(err) = file.write_all(line.as_bytes()).await
            && let Some(sender) = err_tx.take()
        {
            let _ = sender.send(format!("write failed: {err}"));
            continue;
        }
        if let Err(err) = file.flush().await
            && let Some(sender) = err_tx.take()
        {
            let _ = sender.send(format!("flush failed: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_preview_truncates_to_cap() {
        let bytes = vec![b'a'; COMMAND_OUTPUT_PREVIEW_MAX_BYTES + 50];
        let preview = TranscriptRecord::command_preview_from_bytes(&bytes);
        assert_eq!(preview.len(), COMMAND_OUTPUT_PREVIEW_MAX_BYTES);
    }

    #[test]
    fn command_preview_shorter_than_cap_kept_as_is() {
        let preview = TranscriptRecord::command_preview_from_bytes(b"hello");
        assert_eq!(preview, "hello");
    }

    #[test]
    fn approval_decision_strings_are_stable() {
        assert_eq!(ApprovalDecision::Approve.as_str(), "approve");
        assert_eq!(ApprovalDecision::Reject.as_str(), "reject");
    }

    #[test]
    fn session_end_reason_strings_are_stable() {
        assert_eq!(SessionEndReason::UserQuit.as_str(), "user_quit");
        assert_eq!(SessionEndReason::Eof.as_str(), "eof");
    }
}
