//! Shell command executor for the `command` tool.
//!
//! Spawns `/bin/sh -c <command_line>` under its own process group
//! (`setsid`), streams sanitized stdout / stderr chunks back to the
//! shell, and terminates the group on timeout / cancel / output
//! limit via `killpg`. The child never inherits the TUI's stdin.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nojson::{DisplayJson, Json, JsonFormatter};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Notify, mpsc};

use crate::sansio::agent::{
    COMMAND_KILL_GRACE_MS, COMMAND_MAX_STREAM_BYTES, COMMAND_STREAM_CHUNK_SIZE, CommandInvocation,
    CommandOutputStream,
};

/// One sanitized output chunk delivered to the shell for TUI display
/// and forwarding to the core as
/// [`crate::sansio::agent::Event::CommandOutputChunk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutputMessage {
    pub stream: CommandOutputStream,
    pub bytes: Vec<u8>,
}

/// Why the child ended. Recorded verbatim into the tool result JSON
/// so the model can see what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTerminationReason {
    Exited,
    Timeout,
    StdoutLimit,
    StderrLimit,
    Cancelled,
    KilledBySignal,
}

impl CommandTerminationReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Timeout => "timeout",
            Self::StdoutLimit => "stdout_limit",
            Self::StderrLimit => "stderr_limit",
            Self::Cancelled => "cancelled",
            Self::KilledBySignal => "killed_by_signal",
        }
    }
}

/// Final outcome of a command execution. Rendered into the
/// `Tool` role message with [`Self::to_json_string`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub termination_reason: CommandTerminationReason,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl CommandResult {
    pub fn to_json_string(&self) -> String {
        Json(self).to_string()
    }
}

impl DisplayJson for CommandResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            match self.exit_code {
                Some(code) => f.member("exit_code", code)?,
                None => f.member("exit_code", Option::<i32>::None)?,
            }
            f.member("termination_reason", self.termination_reason.as_str())?;
            f.member("duration_ms", self.duration_ms)?;
            f.member("stdout", &self.stdout)?;
            f.member("stdout_truncated", self.stdout_truncated)?;
            f.member("stderr", &self.stderr)?;
            f.member("stderr_truncated", self.stderr_truncated)
        })
    }
}

/// Spawn `/bin/sh -c <invocation.command_line>` in `cwd`, stream
/// sanitized output to `output_tx`, and resolve to a
/// [`CommandResult`] when the child exits, is killed, or hits a
/// limit. `cancel.notified()` is polled continuously; the first
/// notification triggers a SIGTERM → grace → SIGKILL kill sequence.
///
/// Returns `Err(io::Error)` only if the initial spawn fails; every
/// other outcome (non-zero exit, kill, timeout, ...) is carried by
/// the returned `CommandResult`.
pub async fn run_command(
    cwd: PathBuf,
    invocation: CommandInvocation,
    cancel: Arc<Notify>,
    output_tx: mpsc::UnboundedSender<CommandOutputMessage>,
) -> std::io::Result<CommandResult> {
    let started = Instant::now();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(&invocation.command_line)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid() is async-signal-safe (POSIX). It puts the
    // child in its own process group / session so killpg(pgid, sig)
    // reaches every grandchild.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let child_pid = child.id().expect("child pid after spawn") as libc::pid_t;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_accum = StreamAccumulator::new(CommandOutputStream::Stdout);
    let mut stderr_accum = StreamAccumulator::new(CommandOutputStream::Stderr);
    let timeout_dur = Duration::from_secs(invocation.timeout_seconds);

    let mut reader_stdout = ChunkedReader::new(stdout);
    let mut reader_stderr = ChunkedReader::new(stderr);

    let termination = loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break CommandTerminationReason::Cancelled,
            _ = tokio::time::sleep(timeout_dur) => break CommandTerminationReason::Timeout,
            chunk = reader_stdout.next() => {
                if let Some(bytes) = chunk? {
                    match stdout_accum.push(bytes, &output_tx) {
                        StreamStatus::Ok => continue,
                        StreamStatus::LimitReached => break CommandTerminationReason::StdoutLimit,
                    }
                } else {
                    reader_stdout.close();
                }
            }
            chunk = reader_stderr.next() => {
                if let Some(bytes) = chunk? {
                    match stderr_accum.push(bytes, &output_tx) {
                        StreamStatus::Ok => continue,
                        StreamStatus::LimitReached => break CommandTerminationReason::StderrLimit,
                    }
                } else {
                    reader_stderr.close();
                }
            }
            status = child.wait(), if reader_stdout.is_closed() && reader_stderr.is_closed() => {
                let status = status?;
                let reason = if let Some(code) = status.code() {
                    let _ = code;
                    CommandTerminationReason::Exited
                } else {
                    CommandTerminationReason::KilledBySignal
                };
                return Ok(finalize(
                    started,
                    status.code(),
                    reason,
                    stdout_accum,
                    stderr_accum,
                ));
            }
        }
    };

    // A break out of the loop means we (attini) decided to end the
    // child. Kill the group and wait for the exit to reap the zombie.
    kill_group(child_pid);
    let exit = wait_with_grace(&mut child).await;
    let code = exit.as_ref().ok().and_then(|s| s.code());
    Ok(finalize(
        started,
        code,
        termination,
        stdout_accum,
        stderr_accum,
    ))
}

fn finalize(
    started: Instant,
    exit_code: Option<i32>,
    termination_reason: CommandTerminationReason,
    stdout: StreamAccumulator,
    stderr: StreamAccumulator,
) -> CommandResult {
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    CommandResult {
        exit_code,
        termination_reason,
        duration_ms,
        stdout: stdout.text,
        stdout_truncated: stdout.truncated,
        stderr: stderr.text,
        stderr_truncated: stderr.truncated,
    }
}

fn kill_group(pid: libc::pid_t) {
    // pid was returned by setsid(), which sets pgid = pid. killpg
    // requires a positive process group id (POSIX).
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
    }
}

async fn wait_with_grace(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    let grace = Duration::from_millis(COMMAND_KILL_GRACE_MS);
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(res) => res,
        Err(_) => {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            child.wait().await
        }
    }
}

// -------------------------------------------------------------
// stream reading & accumulation
// -------------------------------------------------------------

/// Async reader that yields fixed-size sanitized chunks. Wraps a
/// tokio pipe and applies [`Sanitizer`] before handing bytes to the
/// accumulator / output_tx.
struct ChunkedReader<R> {
    inner: Option<R>,
    sanitizer: Sanitizer,
    buf: Vec<u8>,
}

impl<R: AsyncReadExt + Unpin> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: Some(inner),
            sanitizer: Sanitizer::new(),
            buf: vec![0u8; COMMAND_STREAM_CHUNK_SIZE],
        }
    }

    async fn next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let inner = match self.inner.as_mut() {
            Some(inner) => inner,
            None => return std::future::pending().await,
        };
        let n = inner.read(&mut self.buf).await?;
        if n == 0 {
            self.inner = None;
            return Ok(None);
        }
        let sanitized = self.sanitizer.feed(&self.buf[..n]);
        Ok(Some(sanitized))
    }

    fn close(&mut self) {
        self.inner = None;
    }

    fn is_closed(&self) -> bool {
        self.inner.is_none()
    }
}

enum StreamStatus {
    Ok,
    LimitReached,
}

struct StreamAccumulator {
    stream: CommandOutputStream,
    text: String,
    bytes_written: usize,
    truncated: bool,
}

impl StreamAccumulator {
    fn new(stream: CommandOutputStream) -> Self {
        Self {
            stream,
            text: String::new(),
            bytes_written: 0,
            truncated: false,
        }
    }

    fn push(
        &mut self,
        bytes: Vec<u8>,
        output_tx: &mpsc::UnboundedSender<CommandOutputMessage>,
    ) -> StreamStatus {
        let remaining = COMMAND_MAX_STREAM_BYTES.saturating_sub(self.bytes_written);
        let (accepted, limit_reached) = if bytes.len() <= remaining {
            (bytes, false)
        } else {
            let cut = bytes[..remaining].to_vec();
            self.truncated = true;
            (cut, true)
        };
        if !accepted.is_empty() {
            self.bytes_written += accepted.len();
            match std::str::from_utf8(&accepted) {
                Ok(s) => self.text.push_str(s),
                Err(_) => {
                    let lossy = String::from_utf8_lossy(&accepted);
                    self.text.push_str(&lossy);
                }
            }
            let _ = output_tx.send(CommandOutputMessage {
                stream: self.stream,
                bytes: accepted,
            });
        }
        if limit_reached {
            StreamStatus::LimitReached
        } else {
            StreamStatus::Ok
        }
    }
}

// -------------------------------------------------------------
// sanitizer
// -------------------------------------------------------------

/// Strips ANSI escape sequences, C0 controls (except LF/CR/TAB),
/// C1 controls, and Unicode BiDi override codepoints. State is
/// preserved between chunks so escapes split across reads still
/// get consumed correctly.
struct Sanitizer {
    state: SanitizerState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SanitizerState {
    Ground,
    /// Saw ESC (0x1B). Next byte determines the sequence type.
    AfterEsc,
    /// Inside CSI (`ESC [`). Waiting for a terminator in 0x40..=0x7E.
    Csi,
    /// Inside OSC / DCS / APC / PM / SOS. Waiting for `ESC \` (ST)
    /// or `BEL` (0x07).
    String,
    /// Inside a `String` state and just saw ESC — the next byte
    /// completes the ST terminator if it is `\` (0x5C).
    StringEscape,
}

impl Sanitizer {
    fn new() -> Self {
        Self {
            state: SanitizerState::Ground,
        }
    }

    /// Feed raw bytes, return sanitized UTF-8 bytes.
    ///
    /// The pipeline runs in two phases so C1 controls
    /// (0x80..=0x9F) do not collide with UTF-8 continuation bytes:
    ///
    /// 1. Byte-level ANSI escape stripper (state machine, ASCII-only).
    /// 2. UTF-8 lossy decode + char-level control / BiDi filter.
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut stripped = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.step(b, &mut stripped);
        }
        let text = String::from_utf8_lossy(&stripped);
        text.chars()
            .filter(|&c| char_should_keep(c))
            .collect::<String>()
            .into_bytes()
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        use SanitizerState::*;
        match self.state {
            Ground => match b {
                0x1B => self.state = AfterEsc,
                _ => out.push(b),
            },
            AfterEsc => match b {
                b'[' => self.state = Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = String,
                b'\\' => self.state = Ground, // stray ST
                _ => self.state = Ground,     // 2-byte ESC ?; ESC seq consumed
            },
            Csi => {
                if (0x40..=0x7E).contains(&b) {
                    self.state = Ground;
                }
                // else stay in CSI (parameter / intermediate byte)
            }
            String => match b {
                0x07 => self.state = Ground,
                0x1B => self.state = StringEscape,
                _ => {}
            },
            StringEscape => match b {
                b'\\' => self.state = Ground,
                _ => self.state = String,
            },
        }
    }
}

fn char_should_keep(c: char) -> bool {
    match c {
        '\n' | '\r' | '\t' => true,
        _ if (c as u32) < 0x20 => false, // C0 (minus above)
        '\u{007F}' => false,             // DEL
        _ if (0x80..=0x9F).contains(&(c as u32)) => false, // C1
        _ if (0x202A..=0x202E).contains(&(c as u32)) => false, // BiDi override
        _ if (0x2066..=0x2069).contains(&(c as u32)) => false, // BiDi isolate
        _ => true,
    }
}

// -------------------------------------------------------------
// tests
// -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_strips_ansi_csi_but_keeps_text() {
        let mut s = Sanitizer::new();
        let out = s.feed(b"\x1b[31mred\x1b[0m text\n");
        assert_eq!(out, b"red text\n");
    }

    #[test]
    fn sanitizer_strips_c0_except_lf_cr_tab() {
        let mut s = Sanitizer::new();
        let out = s.feed(b"a\x00b\x07c\td\ne\rf\x1bg");
        // NUL and BEL dropped by the char-level filter. ESC starts a
        // 2-byte escape sequence, so `\x1bg` is consumed together;
        // TAB / LF / CR are kept.
        assert_eq!(out, b"abc\td\ne\rf");
    }

    #[test]
    fn sanitizer_preserves_state_across_chunk_boundary() {
        let mut s = Sanitizer::new();
        let first = s.feed(b"before\x1b");
        assert_eq!(first, b"before");
        let second = s.feed(b"[31mafter");
        assert_eq!(second, b"after");
    }

    #[test]
    fn sanitizer_strips_bidi_override() {
        let mut s = Sanitizer::new();
        let out = s.feed("safe\u{202E}reversed".as_bytes());
        assert_eq!(std::str::from_utf8(&out).unwrap(), "safereversed");
    }

    #[test]
    fn command_result_to_json_shape() {
        let r = CommandResult {
            exit_code: Some(0),
            termination_reason: CommandTerminationReason::Exited,
            duration_ms: 42,
            stdout: "hi\n".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let s = r.to_json_string();
        assert!(s.contains(r#""exit_code":0"#), "s={s}");
        assert!(s.contains(r#""termination_reason":"exited""#));
        assert!(s.contains(r#""duration_ms":42"#));
        assert!(s.contains(r#""stdout":"hi\n""#));
        assert!(s.contains(r#""stdout_truncated":false"#));
    }

    #[test]
    fn command_result_to_json_encodes_null_exit_code() {
        let r = CommandResult {
            exit_code: None,
            termination_reason: CommandTerminationReason::Cancelled,
            duration_ms: 5,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let s = r.to_json_string();
        assert!(s.contains(r#""exit_code":null"#), "s={s}");
        assert!(s.contains(r#""termination_reason":"cancelled""#));
    }
}
