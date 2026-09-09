//! Run a child process while accumulating up to
//! [`COMMAND_MAX_STREAM_BYTES`] of each stream for the tool result,
//! and streaming the child's output to the parent's stderr under a
//! byte-rate limit **when stderr is a terminal**. The streaming is a
//! human-watching copy only: when stderr is not a terminal (a pipe, a
//! test harness, CI) no display bytes are written, which avoids
//! back-pressure that could otherwise stall the child if the retained
//! output is large. Accumulation is bounded by
//! [`COMMAND_MAX_STREAM_BYTES`], and when a stream exceeds that the
//! retained buffer is truncated with [`ChildOutput::truncated`] set.
//! The parent's stdout is never used here because the CLI's final
//! result is emitted there.
//!
//! The child's output is framed by `[child] started (pid ...)` /
//! `[child] finished (...)` separator lines on stderr, and when stderr
//! is a terminal the streamed bytes are coloured (dim for the child's
//! stdout, yellow for its stderr) so the output is clearly marked as
//! coming from the child process.

use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on bytes of a child stream displayed to the parent's
/// stderr per one-second window. Excess bytes are still accumulated.
pub const STREAM_DISPLAY_BYTES_PER_SECOND: usize = 64 * 1024;

/// Maximum bytes retained from either the child's stdout or stderr
/// for the tool result. When a stream exceeds this, the retained
/// buffer is truncated and [`ChildOutput::truncated`] is set.
pub const COMMAND_MAX_STREAM_BYTES: usize = 256 * 1024;

/// Size of each read from a child pipe.
const CHUNK_SIZE: usize = 4 * 1024;

/// ANSI escape for dimmed text (used for the child's stdout).
const ANSI_DIM: &str = "\x1b[2m";
/// ANSI escape for yellow text (used for the child's stderr).
const ANSI_YELLOW: &str = "\x1b[33m";
/// ANSI escape resetting all attributes.
const ANSI_RESET: &str = "\x1b[0m";

/// Captured result of a streamed child run.
#[derive(Debug)]
pub struct ChildOutput {
    /// Full accumulated stdout (lossy UTF-8, matching the previous
    /// `Command::output` behaviour).
    pub stdout: String,
    /// Full accumulated stderr (lossy UTF-8).
    pub stderr: String,
    pub status: ExitStatus,
    /// Wall time from spawn to both streams drained.
    pub duration: Duration,
    /// True when either stream was truncated at
    /// [`COMMAND_MAX_STREAM_BYTES`]; the stdio text is incomplete.
    pub truncated: bool,
}

/// Run `cmd` with piped stdout / stderr, streaming both to the parent
/// stderr under [`STREAM_DISPLAY_BYTES_PER_SECOND`] while accumulating
/// the full output. Blocks until the child exits and both pipes are
/// drained. Prints `[child] started` / `[child] finished` separator
/// lines to stderr, and colours the streamed bytes when stderr is a
/// terminal.
pub fn run_streamed(cmd: &mut Command) -> io::Result<ChildOutput> {
    let started = Instant::now();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let color = io::stderr().is_terminal();
    eprintln!("{}", start_line(pid));

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_thread = stdout.map(|reader| {
        let limiter = DisplayRateLimiter::new_per_second();
        thread::spawn(move || {
            let mut sink = io::stderr().lock();
            pump(reader, StreamKind::Stdout, color, limiter, &mut sink)
        })
    });
    let stderr_thread = stderr.map(|reader| {
        let limiter = DisplayRateLimiter::new_per_second();
        thread::spawn(move || {
            let mut sink = io::stderr().lock();
            pump(reader, StreamKind::Stderr, color, limiter, &mut sink)
        })
    });

    let status = child.wait()?;
    let (stdout_bytes, stdout_truncated) = join_pump(stdout_thread)?;
    let (stderr_bytes, stderr_truncated) = join_pump(stderr_thread)?;
    let duration = started.elapsed();
    eprintln!("{}", finish_line(&status, duration));

    Ok(ChildOutput {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        status,
        duration,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// `[child] started (pid NNNN)` separator line.
pub fn start_line(pid: u32) -> String {
    format!("[child] started (pid {pid})")
}

/// `[child] finished (exit 0, 1.2s)` separator line. A child that was
/// terminated by a signal is reported as `signal`.
pub fn finish_line(status: &ExitStatus, duration: Duration) -> String {
    let termination = match status.code() {
        Some(code) => format!("exit {code}"),
        None => "signal".to_string(),
    };
    format!(
        "[child] finished ({termination}, {:.1}s)",
        duration.as_secs_f64()
    )
}

/// Result of draining one child stream: the retained (possibly
/// truncated) bytes and whether truncation happened.
type Pumped = (Vec<u8>, bool);

fn join_pump(handle: Option<thread::JoinHandle<io::Result<Pumped>>>) -> io::Result<Pumped> {
    match handle {
        Some(h) => h
            .join()
            .map_err(|_| io::Error::other("child output reader thread panicked"))?,
        None => Ok((Vec::new(), false)),
    }
}

/// Which child stream a pump thread is draining. Selects the display
/// colour when the parent stderr is a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    fn ansi(self) -> &'static str {
        match self {
            Self::Stdout => ANSI_DIM,
            Self::Stderr => ANSI_YELLOW,
        }
    }
}

/// Read a child pipe to EOF, accumulating every byte and displaying
/// the stream to the parent stderr under `limiter`'s rate cap, wrapped
/// in the stream's ANSI colour when `color` is true.
fn pump<R: Read, W: Write>(
    mut reader: R,
    kind: StreamKind,
    color: bool,
    mut limiter: DisplayRateLimiter,
    sink: &mut W,
) -> io::Result<Pumped> {
    let mut accumulated = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; CHUNK_SIZE];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let remaining = COMMAND_MAX_STREAM_BYTES.saturating_sub(accumulated.len());
        if remaining > 0 {
            let take = n.min(remaining);
            accumulated.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
        if color {
            let allowed = limiter.allow(&chunk[..n]);
            if allowed > 0 {
                write_display(sink, kind, color, &chunk[..allowed])?;
            }
        }
    }
    Ok((accumulated, truncated))
}

/// Write `bytes` to `sink`, wrapping them in the stream colour when
/// `color` is true (each write is self-contained: open colour, bytes,
/// reset), and raw otherwise.
fn write_display<W: Write>(
    sink: &mut W,
    kind: StreamKind,
    color: bool,
    bytes: &[u8],
) -> io::Result<()> {
    if color {
        sink.write_all(kind.ansi().as_bytes())?;
        sink.write_all(bytes)?;
        sink.write_all(ANSI_RESET.as_bytes())
    } else {
        sink.write_all(bytes)
    }
}

/// Per-stream byte-rate limiter for the display copy. Accumulation is
/// unaffected: only the displayed prefix of each chunk is capped.
#[derive(Debug, Clone)]
pub struct DisplayRateLimiter {
    budget: usize,
    window: Duration,
    window_start: Instant,
    used: usize,
}

impl DisplayRateLimiter {
    pub fn new(budget: usize, window: Duration) -> Self {
        Self {
            budget,
            window,
            window_start: Instant::now(),
            used: 0,
        }
    }

    pub fn new_per_second() -> Self {
        Self::new(STREAM_DISPLAY_BYTES_PER_SECOND, Duration::from_secs(1))
    }

    /// Return how many bytes of `bytes` may be displayed in the
    /// current window (up to the remaining budget), advancing the
    /// window when it has elapsed. The returned prefix is what callers
    /// should display; the rest is dropped from the display only.
    pub fn allow(&mut self, bytes: &[u8]) -> usize {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= self.window {
            self.window_start = now;
            self.used = 0;
        }
        let allowed = bytes.len().min(self.budget.saturating_sub(self.used));
        self.used += allowed;
        allowed
    }

    /// Display up to the remaining window budget of `bytes`, writing
    /// to `sink`. Excess bytes are dropped from the display only.
    pub fn write<W: Write>(&mut self, sink: &mut W, bytes: &[u8]) -> io::Result<()> {
        let allowed = self.allow(bytes);
        if allowed > 0 {
            sink.write_all(&bytes[..allowed])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bytes(limiter: &mut DisplayRateLimiter, sink: &mut Vec<u8>, bytes: &[u8]) {
        limiter.write(sink, bytes).expect("write");
    }

    #[test]
    fn limiter_caps_display_at_window_budget() {
        let mut limiter = DisplayRateLimiter::new(4, Duration::from_secs(1));
        let mut sink = Vec::new();
        let payload = b"abcdefghij";
        write_bytes(&mut limiter, &mut sink, payload);
        assert_eq!(sink.len(), 4, "first window caps at budget");
        // The remainder of the chunk is dropped, not carried over.
        write_bytes(&mut limiter, &mut sink, payload);
        assert_eq!(sink.len(), 4, "same window still exhausted");
    }

    #[test]
    fn limiter_resets_after_window_elapses() {
        let mut limiter = DisplayRateLimiter::new(4, Duration::from_millis(1));
        let mut sink = Vec::new();
        write_bytes(&mut limiter, &mut sink, b"aaaa");
        std::thread::sleep(Duration::from_millis(5));
        write_bytes(&mut limiter, &mut sink, b"bbbb");
        assert_eq!(sink.len(), 8, "a new window restores the budget");
    }

    #[test]
    fn limiter_allows_up_to_budget_across_chunks() {
        let mut limiter = DisplayRateLimiter::new(5, Duration::from_secs(1));
        let mut sink = Vec::new();
        write_bytes(&mut limiter, &mut sink, b"ab");
        write_bytes(&mut limiter, &mut sink, b"cde");
        write_bytes(&mut limiter, &mut sink, b"fgh");
        assert_eq!(sink, b"abcde", "partial chunk fills the remaining budget");
    }

    #[test]
    fn pump_accumulates_full_output_under_small_budget() {
        // A tiny budget must not truncate the accumulated bytes.
        let reader = io::Cursor::new(b"x".repeat(10 * 1024));
        let limiter = DisplayRateLimiter::new(1, Duration::from_secs(1));
        let mut sink = io::sink();
        let (bytes, truncated) =
            pump(reader, StreamKind::Stdout, false, limiter, &mut sink).expect("pump");
        assert_eq!(bytes.len(), 10 * 1024, "accumulation is never rate-capped");
        assert!(!truncated, "10 KiB is below the stream cap");
    }

    #[test]
    fn pump_truncates_accumulation_at_stream_cap() {
        let payload = vec![b'x'; COMMAND_MAX_STREAM_BYTES + 1];
        let reader = io::Cursor::new(payload);
        let limiter = DisplayRateLimiter::new(COMMAND_MAX_STREAM_BYTES, Duration::from_secs(1));
        let mut sink = io::sink();
        let (bytes, truncated) =
            pump(reader, StreamKind::Stdout, false, limiter, &mut sink).expect("pump");
        assert_eq!(
            bytes.len(),
            COMMAND_MAX_STREAM_BYTES,
            "retained output hits the cap"
        );
        assert!(truncated);
    }

    #[test]
    fn run_streamed_truncates_large_output_at_stream_cap() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes x | head -c 300000");
        let out = run_streamed(&mut cmd).expect("run");
        assert!(out.status.success());
        assert_eq!(
            out.stdout.len(),
            COMMAND_MAX_STREAM_BYTES,
            "retained stdout hits the stream cap"
        );
        assert!(out.truncated);
    }

    #[test]
    fn run_streamed_captures_full_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'out-line1\\nout-line2\\n'; printf 'err-line\\n' >&2");
        let out = run_streamed(&mut cmd).expect("run");
        assert!(out.status.success());
        assert_eq!(out.stdout, "out-line1\nout-line2\n");
        assert_eq!(out.stderr, "err-line\n");
    }

    #[test]
    fn run_streamed_non_zero_exit_is_not_an_error() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 3");
        let out = run_streamed(&mut cmd).expect("run");
        assert_eq!(out.status.code(), Some(3));
    }

    // -----------------------------------------------------------------
    // Separator lines and colouring
    // -----------------------------------------------------------------

    #[test]
    fn start_line_contains_pid() {
        assert_eq!(start_line(12345), "[child] started (pid 12345)");
    }

    #[test]
    fn finish_line_reports_exit_code_and_duration() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .expect("run");
        assert_eq!(
            finish_line(&status, Duration::from_millis(1250)),
            "[child] finished (exit 0, 1.2s)"
        );
    }

    #[test]
    fn finish_line_reports_signal_without_exit_code() {
        // A status whose code() is None is treated as signalled.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("kill -9 $$")
            .status()
            .expect("run");
        assert!(status.code().is_none());
        let line = finish_line(&status, Duration::ZERO);
        assert!(
            line.starts_with("[child] finished (signal, 0.0s)"),
            "{line}"
        );
    }

    #[test]
    fn write_display_wraps_in_colour_when_enabled() {
        let mut sink = Vec::new();
        write_display(&mut sink, StreamKind::Stdout, true, b"abc").expect("write");
        assert_eq!(sink, b"\x1b[2mabc\x1b[0m");

        let mut sink = Vec::new();
        write_display(&mut sink, StreamKind::Stderr, true, b"abc").expect("write");
        assert_eq!(sink, b"\x1b[33mabc\x1b[0m");
    }

    #[test]
    fn write_display_is_raw_when_colour_disabled() {
        let mut sink = Vec::new();
        write_display(&mut sink, StreamKind::Stdout, false, b"abc").expect("write");
        write_display(&mut sink, StreamKind::Stderr, false, b"def").expect("write");
        assert_eq!(sink, b"abcdef", "no ANSI codes outside a terminal");
    }

    #[test]
    fn limiter_allow_returns_displayable_count() {
        let mut limiter = DisplayRateLimiter::new(5, Duration::from_secs(1));
        assert_eq!(limiter.allow(b"ab"), 2);
        assert_eq!(limiter.allow(b"cdefgh"), 3, "fills the remaining budget");
        assert_eq!(limiter.allow(b"ij"), 0, "budget exhausted");
    }
}
